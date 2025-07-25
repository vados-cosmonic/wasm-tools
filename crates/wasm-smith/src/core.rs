//! Generating arbitrary core Wasm modules.

mod code_builder;
pub(crate) mod encode;
mod terminate;

use crate::{Config, arbitrary_loop, limited_string, unique_string};
use arbitrary::{Arbitrary, Result, Unstructured};
use code_builder::CodeBuilderAllocations;
use flagset::{FlagSet, flags};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::mem;
use std::ops::Range;
use std::rc::Rc;
use std::str::{self, FromStr};
use wasm_encoder::{
    AbstractHeapType, ArrayType, BlockType, ConstExpr, ExportKind, FieldType, HeapType, RefType,
    StorageType, StructType, ValType,
};
pub(crate) use wasm_encoder::{GlobalType, MemoryType, TableType};

// NB: these constants are used to control the rate at which various events
// occur. For more information see where these constants are used. Their values
// are somewhat random in the sense that they're not scientifically determined
// or anything like that, I just threw a bunch of random data at wasm-smith and
// measured various rates of ooms/traps/etc and adjusted these so abnormal
// events were ~1% of the time.
const CHANCE_OFFSET_INBOUNDS: usize = 10; // bigger = less traps
const CHANCE_SEGMENT_ON_EMPTY: usize = 10; // bigger = less traps
const PCT_INBOUNDS: f64 = 0.995; // bigger = less traps

type Instruction = wasm_encoder::Instruction<'static>;

/// A pseudo-random WebAssembly module.
///
/// Construct instances of this type (with default configuration) with [the
/// `Arbitrary`
/// trait](https://docs.rs/arbitrary/*/arbitrary/trait.Arbitrary.html).
///
/// ## Configuring Generated Modules
///
/// To configure the shape of generated module, create a
/// [`Config`][crate::Config] and then call [`Module::new`][crate::Module::new]
/// with it.
pub struct Module {
    config: Config,
    duplicate_imports_behavior: DuplicateImportsBehavior,
    valtypes: Vec<ValType>,

    /// All types locally defined in this module (available in the type index
    /// space).
    types: Vec<SubType>,

    /// Non-overlapping ranges within `types` that belong to the same rec
    /// group. All of `types` is covered by these ranges. When GC is not
    /// enabled, these are all single-element ranges.
    rec_groups: Vec<Range<usize>>,

    /// A map from a super type to all of its sub types.
    super_to_sub_types: HashMap<u32, Vec<u32>>,

    /// Indices within `types` that are not final types.
    can_subtype: Vec<u32>,

    /// Whether we should encode a types section, even if `self.types` is empty.
    should_encode_types: bool,

    /// Whether we should propagate sharedness to types generated inside
    /// `propagate_shared`.
    must_share: bool,

    /// All of this module's imports. These don't have their own index space,
    /// but instead introduce entries to each imported entity's associated index
    /// space.
    imports: Vec<Import>,

    /// Whether we should encode an imports section, even if `self.imports` is
    /// empty.
    should_encode_imports: bool,

    /// Indices within `types` that are array types.
    array_types: Vec<u32>,

    /// Indices within `types` that are function types.
    func_types: Vec<u32>,

    /// Indices within `types that are struct types.
    struct_types: Vec<u32>,

    /// Number of imported items into this module.
    num_imports: usize,

    /// The number of tags defined in this module (not imported or
    /// aliased).
    num_defined_tags: usize,

    /// The number of functions defined in this module (not imported or
    /// aliased).
    num_defined_funcs: usize,

    /// Initialization expressions for all defined tables in this module.
    defined_tables: Vec<Option<ConstExpr>>,

    /// The number of memories defined in this module (not imported or
    /// aliased).
    num_defined_memories: usize,

    /// The indexes and initialization expressions of globals defined in this
    /// module.
    defined_globals: Vec<(u32, ConstExpr)>,

    /// All tags available to this module, sorted by their index. The list
    /// entry is the type of each tag.
    tags: Vec<TagType>,

    /// All functions available to this module, sorted by their index. The list
    /// entry points to the index in this module where the function type is
    /// defined (if available) and provides the type of the function.
    funcs: Vec<(u32, Rc<FuncType>)>,

    /// All tables available to this module, sorted by their index. The list
    /// entry is the type of each table.
    tables: Vec<TableType>,

    /// All globals available to this module, sorted by their index. The list
    /// entry is the type of each global.
    globals: Vec<GlobalType>,

    /// All memories available to this module, sorted by their index. The list
    /// entry is the type of each memory.
    memories: Vec<MemoryType>,

    exports: Vec<(String, ExportKind, u32)>,
    start: Option<u32>,
    elems: Vec<ElementSegment>,
    code: Vec<Code>,
    data: Vec<DataSegment>,

    /// The predicted size of the effective type of this module, based on this
    /// module's size of the types of imports/exports.
    type_size: u32,

    /// Names currently exported from this module.
    export_names: HashSet<String>,

    /// Reusable buffer in `self.arbitrary_const_expr` to amortize the cost of
    /// allocation.
    const_expr_choices: Vec<Box<dyn Fn(&mut Unstructured, ValType) -> Result<ConstExpr>>>,

    /// What the maximum type index that can be referenced is.
    max_type_limit: MaxTypeLimit,

    /// Some known-interesting values, such as powers of two, values just before
    /// or just after a memory size, etc...
    interesting_values32: Vec<u32>,
    interesting_values64: Vec<u64>,
}

impl<'a> Arbitrary<'a> for Module {
    fn arbitrary(u: &mut Unstructured<'a>) -> Result<Self> {
        Module::new(Config::default(), u)
    }
}

impl fmt::Debug for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Module")
            .field("config", &self.config)
            .field(&"...", &"...")
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicateImportsBehavior {
    Allowed,
    Disallowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AllowEmptyRecGroup {
    Yes,
    No,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaxTypeLimit {
    ModuleTypes,
    Num(u32),
}

impl Module {
    /// Returns a reference to the internal configuration.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Creates a new `Module` with the specified `config` for
    /// configuration and `Unstructured` for the DNA of this module.
    pub fn new(config: Config, u: &mut Unstructured<'_>) -> Result<Self> {
        Self::new_internal(config, u, DuplicateImportsBehavior::Allowed)
    }

    pub(crate) fn new_internal(
        config: Config,
        u: &mut Unstructured<'_>,
        duplicate_imports_behavior: DuplicateImportsBehavior,
    ) -> Result<Self> {
        let mut module = Module::empty(config, duplicate_imports_behavior);
        module.build(u)?;
        Ok(module)
    }

    fn empty(mut config: Config, duplicate_imports_behavior: DuplicateImportsBehavior) -> Self {
        config.sanitize();
        Module {
            config,
            duplicate_imports_behavior,
            valtypes: Vec::new(),
            types: Vec::new(),
            rec_groups: Vec::new(),
            can_subtype: Vec::new(),
            super_to_sub_types: HashMap::new(),
            should_encode_types: false,
            imports: Vec::new(),
            should_encode_imports: false,
            array_types: Vec::new(),
            func_types: Vec::new(),
            struct_types: Vec::new(),
            num_imports: 0,
            num_defined_tags: 0,
            num_defined_funcs: 0,
            defined_tables: Vec::new(),
            num_defined_memories: 0,
            defined_globals: Vec::new(),
            tags: Vec::new(),
            funcs: Vec::new(),
            tables: Vec::new(),
            globals: Vec::new(),
            memories: Vec::new(),
            exports: Vec::new(),
            start: None,
            elems: Vec::new(),
            code: Vec::new(),
            data: Vec::new(),
            type_size: 0,
            export_names: HashSet::new(),
            const_expr_choices: Vec::new(),
            max_type_limit: MaxTypeLimit::ModuleTypes,
            interesting_values32: Vec::new(),
            interesting_values64: Vec::new(),
            must_share: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct SubType {
    pub(crate) is_final: bool,
    pub(crate) supertype: Option<u32>,
    pub(crate) composite_type: CompositeType,
    /// How "deep" this subtype's supertype hierarchy is. The base case is 1 and
    /// if `supertype` is present it's `1 + supertype.depth`.
    depth: u32,
}

impl SubType {
    fn unwrap_struct(&self) -> &StructType {
        self.composite_type.unwrap_struct()
    }

    fn unwrap_func(&self) -> &Rc<FuncType> {
        self.composite_type.unwrap_func()
    }

    fn unwrap_array(&self) -> &ArrayType {
        self.composite_type.unwrap_array()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CompositeType {
    pub inner: CompositeInnerType,
    pub shared: bool,
}

impl CompositeType {
    #[cfg(any(feature = "component-model", feature = "wasmparser"))]
    pub(crate) fn new_func(func: Rc<FuncType>, shared: bool) -> Self {
        Self {
            inner: CompositeInnerType::Func(func),
            shared,
        }
    }

    fn unwrap_func(&self) -> &Rc<FuncType> {
        match &self.inner {
            CompositeInnerType::Func(f) => f,
            _ => panic!("not a func"),
        }
    }

    fn unwrap_array(&self) -> &ArrayType {
        match &self.inner {
            CompositeInnerType::Array(a) => a,
            _ => panic!("not an array"),
        }
    }

    fn unwrap_struct(&self) -> &StructType {
        match &self.inner {
            CompositeInnerType::Struct(s) => s,
            _ => panic!("not a struct"),
        }
    }
}

impl From<&CompositeType> for wasm_encoder::CompositeType {
    fn from(ty: &CompositeType) -> Self {
        let inner = match &ty.inner {
            CompositeInnerType::Array(a) => wasm_encoder::CompositeInnerType::Array(*a),
            CompositeInnerType::Func(f) => wasm_encoder::CompositeInnerType::Func(
                wasm_encoder::FuncType::new(f.params.iter().cloned(), f.results.iter().cloned()),
            ),
            CompositeInnerType::Struct(s) => wasm_encoder::CompositeInnerType::Struct(s.clone()),
        };
        wasm_encoder::CompositeType {
            shared: ty.shared,
            inner,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum CompositeInnerType {
    Array(ArrayType),
    Func(Rc<FuncType>),
    Struct(StructType),
}

/// A function signature.
#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct FuncType {
    /// Types of the parameter values.
    pub(crate) params: Vec<ValType>,
    /// Types of the result values.
    pub(crate) results: Vec<ValType>,
}

/// An import of an entity provided externally or by a component.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Import {
    /// The name of the module providing this entity.
    pub(crate) module: String,
    /// The name of the entity.
    pub(crate) field: String,
    /// The type of this entity.
    pub(crate) entity_type: EntityType,
}

/// Type of an entity.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum EntityType {
    /// A global entity.
    Global(GlobalType),
    /// A table entity.
    Table(TableType),
    /// A memory entity.
    Memory(MemoryType),
    /// A tag entity.
    Tag(TagType),
    /// A function entity.
    Func(u32, Rc<FuncType>),
}

/// Type of a tag.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct TagType {
    /// Index of the function type.
    func_type_idx: u32,
    /// Type of the function.
    func_type: Rc<FuncType>,
}

#[derive(Debug)]
struct ElementSegment {
    kind: ElementKind,
    ty: RefType,
    items: Elements,
}

#[derive(Debug)]
enum ElementKind {
    Passive,
    Declared,
    Active {
        table: Option<u32>, // None == table 0 implicitly
        offset: Offset,
    },
}

#[derive(Debug)]
enum Elements {
    Functions(Vec<u32>),
    Expressions(Vec<ConstExpr>),
}

#[derive(Debug)]
struct Code {
    locals: Vec<ValType>,
    instructions: Instructions,
}

#[derive(Debug)]
enum Instructions {
    Generated(Vec<Instruction>),
    Arbitrary(Vec<u8>),
}

#[derive(Debug)]
struct DataSegment {
    kind: DataSegmentKind,
    init: Vec<u8>,
}

#[derive(Debug)]
enum DataSegmentKind {
    Passive,
    Active { memory_index: u32, offset: Offset },
}

#[derive(Debug)]
pub(crate) enum Offset {
    Const32(i32),
    Const64(i64),
    Global(u32),
}

impl Module {
    fn build(&mut self, u: &mut Unstructured) -> Result<()> {
        self.valtypes = configured_valtypes(&self.config);

        let mut generate_arbitrary_imports = true;
        let mut generate_arbitrary_exports = true;
        if self.imports_exports_from_module_shape(u)? {
            generate_arbitrary_imports = false;
            generate_arbitrary_exports = false;
        }
        // We attempt to figure out our available imports *before* creating the types section here,
        // because the types for the imports are already well-known (specified by the user) and we
        // must have those populated for all function/etc. imports, no matter what.
        //
        // This can affect the available capacity for types and such.
        if self.arbitrary_imports_from_available(u)? {
            generate_arbitrary_imports = false;
        }
        self.arbitrary_types(u)?;
        if generate_arbitrary_imports {
            self.arbitrary_imports(u)?;
        }

        self.should_encode_imports = !self.imports.is_empty() || u.arbitrary()?;

        self.arbitrary_tags(u)?;
        self.arbitrary_funcs(u)?;
        self.arbitrary_tables(u)?;
        self.arbitrary_memories(u)?;
        self.arbitrary_globals(u)?;
        if self.required_exports(u)? {
            generate_arbitrary_exports = false;
        }
        if generate_arbitrary_exports {
            self.arbitrary_exports(u)?;
        }
        self.should_encode_types = !self.types.is_empty() || u.arbitrary()?;
        self.arbitrary_start(u)?;
        self.arbitrary_elems(u)?;
        self.arbitrary_data(u)?;
        self.arbitrary_code(u)?;
        Ok(())
    }

    #[inline]
    fn val_type_is_sub_type(&self, a: ValType, b: ValType) -> bool {
        match (a, b) {
            (a, b) if a == b => true,
            (ValType::Ref(a), ValType::Ref(b)) => self.ref_type_is_sub_type(a, b),
            _ => false,
        }
    }

    /// Is `a` a subtype of `b`?
    fn ref_type_is_sub_type(&self, a: RefType, b: RefType) -> bool {
        if a == b {
            return true;
        }

        if a.nullable && !b.nullable {
            return false;
        }

        self.heap_type_is_sub_type(a.heap_type, b.heap_type)
    }

    fn heap_type_is_sub_type(&self, a: HeapType, b: HeapType) -> bool {
        use AbstractHeapType::*;
        use CompositeInnerType as CT;
        use HeapType as HT;
        match (a, b) {
            (a, b) if a == b => true,

            (
                HT::Abstract {
                    shared: a_shared,
                    ty: a_ty,
                },
                HT::Abstract {
                    shared: b_shared,
                    ty: b_ty,
                },
            ) => {
                a_shared == b_shared
                    && match (a_ty, b_ty) {
                        (Eq | I31 | Struct | Array | None, Any) => true,
                        (I31 | Struct | Array | None, Eq) => true,
                        (NoExtern, Extern) => true,
                        (NoFunc, Func) => true,
                        (None, I31 | Array | Struct) => true,
                        (NoExn, Exn) => true,
                        _ => false,
                    }
            }

            (HT::Concrete(a), HT::Abstract { shared, ty }) => {
                let a_ty = &self.ty(a).composite_type;
                if a_ty.shared != shared {
                    return false;
                }
                match ty {
                    Eq | Any => matches!(a_ty.inner, CT::Array(_) | CT::Struct(_)),
                    Struct => matches!(a_ty.inner, CT::Struct(_)),
                    Array => matches!(a_ty.inner, CT::Array(_)),
                    Func => matches!(a_ty.inner, CT::Func(_)),
                    _ => false,
                }
            }

            (HT::Abstract { shared, ty }, HT::Concrete(b)) => {
                let b_ty = &self.ty(b).composite_type;
                if shared != b_ty.shared {
                    return false;
                }
                match ty {
                    None => matches!(b_ty.inner, CT::Array(_) | CT::Struct(_)),
                    NoFunc => matches!(b_ty.inner, CT::Func(_)),
                    _ => false,
                }
            }

            (HT::Concrete(mut a), HT::Concrete(b)) => loop {
                if a == b {
                    return true;
                }
                if let Some(supertype) = self.ty(a).supertype {
                    a = supertype;
                } else {
                    return false;
                }
            },
        }
    }

    fn arbitrary_types(&mut self, u: &mut Unstructured) -> Result<()> {
        assert!(self.config.min_types <= self.config.max_types);
        while self.types.len() < self.config.min_types {
            self.arbitrary_rec_group(u, AllowEmptyRecGroup::No)?;
        }
        while self.types.len() < self.config.max_types {
            let keep_going = u.arbitrary().unwrap_or(false);
            if !keep_going {
                break;
            }
            self.arbitrary_rec_group(u, AllowEmptyRecGroup::Yes)?;
        }
        Ok(())
    }

    fn add_type(&mut self, ty: SubType) -> u32 {
        let index = u32::try_from(self.types.len()).unwrap();

        if let Some(supertype) = ty.supertype {
            assert_eq!(self.is_shared_type(supertype), ty.composite_type.shared);
            self.super_to_sub_types
                .entry(supertype)
                .or_default()
                .push(index);
        }

        let list = match &ty.composite_type.inner {
            CompositeInnerType::Array(_) => &mut self.array_types,
            CompositeInnerType::Func(_) => &mut self.func_types,
            CompositeInnerType::Struct(_) => &mut self.struct_types,
        };
        list.push(index);

        // Calculate the recursive depth of this type, and if it's beneath a
        // threshold then allow future types to subtype this one. Otherwise this
        // can no longer be subtyped so despite this not being final don't add
        // it to the `can_subtype` list.
        //
        // Note that this limit is intentinally a bit less than the wasm-defined
        // maximum of 63.
        const MAX_SUBTYPING_DEPTH: u32 = 60;
        if !ty.is_final && ty.depth < MAX_SUBTYPING_DEPTH {
            self.can_subtype.push(index);
        }

        self.types.push(ty);
        index
    }

    fn arbitrary_rec_group(
        &mut self,
        u: &mut Unstructured,
        kind: AllowEmptyRecGroup,
    ) -> Result<()> {
        let rec_group_start = self.types.len();

        assert!(matches!(self.max_type_limit, MaxTypeLimit::ModuleTypes));

        if self.config.gc_enabled {
            // With small probability, clone an existing rec group.
            if self.rec_groups.len() > 0 && u.ratio(1, u8::MAX)? {
                return self.clone_rec_group(u, kind);
            }

            // Otherwise, create a new rec group with multiple types inside.
            let max_rec_group_size = self.config.max_types - self.types.len();
            let min_rec_group_size = match kind {
                AllowEmptyRecGroup::Yes => 0,
                AllowEmptyRecGroup::No => 1,
            };
            let rec_group_size = u.int_in_range(min_rec_group_size..=max_rec_group_size)?;
            let type_ref_limit = u32::try_from(self.types.len() + rec_group_size).unwrap();
            self.max_type_limit = MaxTypeLimit::Num(type_ref_limit);
            for _ in 0..rec_group_size {
                let ty = self.arbitrary_sub_type(u)?;
                self.add_type(ty);
            }
        } else {
            let type_ref_limit = u32::try_from(self.types.len()).unwrap();
            self.max_type_limit = MaxTypeLimit::Num(type_ref_limit);
            let ty = self.arbitrary_sub_type(u)?;
            self.add_type(ty);
        }

        self.max_type_limit = MaxTypeLimit::ModuleTypes;

        self.rec_groups.push(rec_group_start..self.types.len());
        Ok(())
    }

    fn clone_rec_group(&mut self, u: &mut Unstructured, kind: AllowEmptyRecGroup) -> Result<()> {
        // Choose an arbitrary rec group to clone, but bail out if the selected
        // rec group isn't valid to clone. For example if empty groups aren't
        // allowed and the selected group is empty, or if cloning the rec group
        // would cause the maximum number of types to be exceeded.
        let group = u.choose(&self.rec_groups)?.clone();
        if group.is_empty() && kind == AllowEmptyRecGroup::No {
            return Ok(());
        }
        if group.len() > self.config.max_types.saturating_sub(self.types.len()) {
            return Ok(());
        }

        // NB: this does *not* guarantee that the cloned rec group will
        // canonicalize the same as the original rec group and be deduplicated.
        // That would require a second pass over the cloned types to rewrite
        // references within the original rec group to be references into the
        // new rec group. That might make sense to do one day, but for now we
        // don't do it. That also means that we can't mark the new types as
        // "subtypes" of the old types and vice versa.
        let new_rec_group_start = self.types.len();
        for index in group {
            let orig_ty_index = u32::try_from(index).unwrap();
            let ty = self.ty(orig_ty_index).clone();
            self.add_type(ty);
        }
        self.rec_groups.push(new_rec_group_start..self.types.len());
        Ok(())
    }

    fn arbitrary_sub_type(&mut self, u: &mut Unstructured) -> Result<SubType> {
        if !self.config.gc_enabled {
            let shared = self.arbitrary_shared(u)?;
            let func_type = self.propagate_shared(shared, |m| m.arbitrary_func_type(u))?;
            let composite_type = CompositeType {
                inner: CompositeInnerType::Func(func_type),
                shared,
            };
            return Ok(SubType {
                is_final: true,
                supertype: None,
                composite_type,
                depth: 1,
            });
        }

        if !self.can_subtype.is_empty() && u.ratio(1, 32_u8)? {
            self.arbitrary_sub_type_of_super_type(u)
        } else {
            Ok(SubType {
                is_final: u.arbitrary()?,
                supertype: None,
                composite_type: self.arbitrary_composite_type(u)?,
                depth: 1,
            })
        }
    }

    fn arbitrary_sub_type_of_super_type(&mut self, u: &mut Unstructured) -> Result<SubType> {
        let supertype = *u.choose(&self.can_subtype)?;
        let mut composite_type = self.types[usize::try_from(supertype).unwrap()]
            .composite_type
            .clone();
        match &mut composite_type.inner {
            CompositeInnerType::Array(a) => {
                a.0 = self.arbitrary_matching_field_type(u, a.0)?;
            }
            CompositeInnerType::Func(f) => {
                *f = self.arbitrary_matching_func_type(u, f)?;
            }
            CompositeInnerType::Struct(s) => {
                *s = self.propagate_shared(composite_type.shared, |m| {
                    m.arbitrary_matching_struct_type(u, s)
                })?;
            }
        }
        Ok(SubType {
            is_final: u.arbitrary()?,
            supertype: Some(supertype),
            composite_type,
            depth: 1 + self.types[supertype as usize].depth,
        })
    }

    fn arbitrary_matching_struct_type(
        &mut self,
        u: &mut Unstructured,
        ty: &StructType,
    ) -> Result<StructType> {
        let len_extra_fields = u.int_in_range(0..=5)?;
        let mut fields = Vec::with_capacity(ty.fields.len() + len_extra_fields);
        for field in ty.fields.iter() {
            fields.push(self.arbitrary_matching_field_type(u, *field)?);
        }
        for _ in 0..len_extra_fields {
            fields.push(self.arbitrary_field_type(u)?);
        }
        Ok(StructType {
            fields: fields.into_boxed_slice(),
        })
    }

    fn arbitrary_matching_field_type(
        &mut self,
        u: &mut Unstructured,
        ty: FieldType,
    ) -> Result<FieldType> {
        if ty.mutable {
            Ok(ty)
        } else {
            Ok(FieldType {
                element_type: self.arbitrary_matching_storage_type(u, ty.element_type)?,
                mutable: false,
            })
        }
    }

    fn arbitrary_matching_storage_type(
        &mut self,
        u: &mut Unstructured,
        ty: StorageType,
    ) -> Result<StorageType> {
        match ty {
            StorageType::I8 => Ok(StorageType::I8),
            StorageType::I16 => Ok(StorageType::I16),
            StorageType::Val(ty) => Ok(StorageType::Val(self.arbitrary_matching_val_type(u, ty)?)),
        }
    }

    fn arbitrary_matching_val_type(
        &mut self,
        u: &mut Unstructured,
        ty: ValType,
    ) -> Result<ValType> {
        match ty {
            ValType::I32 => Ok(ValType::I32),
            ValType::I64 => Ok(ValType::I64),
            ValType::F32 => Ok(ValType::F32),
            ValType::F64 => Ok(ValType::F64),
            ValType::V128 => Ok(ValType::V128),
            ValType::Ref(ty) => Ok(ValType::Ref(self.arbitrary_matching_ref_type(u, ty)?)),
        }
    }

    fn arbitrary_matching_ref_type(&self, u: &mut Unstructured, ty: RefType) -> Result<RefType> {
        Ok(RefType {
            nullable: ty.nullable,
            heap_type: self.arbitrary_matching_heap_type(u, ty.heap_type)?,
        })
    }

    fn arbitrary_matching_heap_type(&self, u: &mut Unstructured, ty: HeapType) -> Result<HeapType> {
        use {AbstractHeapType as AHT, CompositeInnerType as CT, HeapType as HT};

        if !self.config.gc_enabled {
            return Ok(ty);
        }

        let mut choices = vec![ty];
        match ty {
            HT::Abstract { shared, ty } => {
                use AbstractHeapType::*;
                let add_abstract = |choices: &mut Vec<HT>, tys: &[AHT]| {
                    choices.extend(tys.iter().map(|&ty| HT::Abstract { shared, ty }));
                };
                let add_concrete = |choices: &mut Vec<HT>, tys: &[u32]| {
                    choices.extend(
                        tys.iter()
                            .filter(|&&idx| shared == self.is_shared_type(idx))
                            .copied()
                            .map(HT::Concrete),
                    );
                };
                match ty {
                    Any => {
                        add_abstract(&mut choices, &[Eq, Struct, Array, I31, None]);
                        add_concrete(&mut choices, &self.array_types);
                        add_concrete(&mut choices, &self.struct_types);
                    }
                    Eq => {
                        add_abstract(&mut choices, &[Struct, Array, I31, None]);
                        add_concrete(&mut choices, &self.array_types);
                        add_concrete(&mut choices, &self.struct_types);
                    }
                    Struct => {
                        add_abstract(&mut choices, &[Struct, None]);
                        add_concrete(&mut choices, &self.struct_types);
                    }
                    Array => {
                        add_abstract(&mut choices, &[Array, None]);
                        add_concrete(&mut choices, &self.array_types);
                    }
                    I31 => {
                        add_abstract(&mut choices, &[None]);
                    }
                    Func => {
                        add_abstract(&mut choices, &[NoFunc]);
                        add_concrete(&mut choices, &self.func_types);
                    }
                    Extern => {
                        add_abstract(&mut choices, &[NoExtern]);
                    }
                    Exn | NoExn | None | NoExtern | NoFunc | Cont | NoCont => {}
                }
            }
            HT::Concrete(idx) => {
                if let Some(subs) = self.super_to_sub_types.get(&idx) {
                    choices.extend(subs.iter().copied().map(HT::Concrete));
                }
                match self
                    .types
                    .get(usize::try_from(idx).unwrap())
                    .map(|ty| (ty.composite_type.shared, &ty.composite_type.inner))
                {
                    Some((shared, CT::Array(_) | CT::Struct(_))) => choices.push(HT::Abstract {
                        shared,
                        ty: AbstractHeapType::None,
                    }),
                    Some((shared, CT::Func(_))) => choices.push(HT::Abstract {
                        shared,
                        ty: AbstractHeapType::NoFunc,
                    }),
                    None => {
                        // The referenced type might be part of this same rec
                        // group we are currently generating, but not generated
                        // yet. In this case, leave `choices` as it is, and we
                        // will just end up choosing the original type again
                        // down below, which is fine.
                    }
                }
            }
        }
        Ok(*u.choose(&choices)?)
    }

    fn arbitrary_matching_func_type(
        &mut self,
        u: &mut Unstructured,
        ty: &FuncType,
    ) -> Result<Rc<FuncType>> {
        // Note: parameters are contravariant, results are covariant. See
        // https://github.com/bytecodealliance/wasm-tools/blob/0616ef196a183cf137ee06b4a5993b7d590088bf/crates/wasmparser/src/readers/core/types/matches.rs#L137-L174
        // for details.
        let mut params = Vec::with_capacity(ty.params.len());
        for param in &ty.params {
            params.push(self.arbitrary_super_type_of_val_type(u, *param)?);
        }
        let mut results = Vec::with_capacity(ty.results.len());
        for result in &ty.results {
            results.push(self.arbitrary_matching_val_type(u, *result)?);
        }
        Ok(Rc::new(FuncType { params, results }))
    }

    fn arbitrary_super_type_of_val_type(
        &mut self,
        u: &mut Unstructured,
        ty: ValType,
    ) -> Result<ValType> {
        match ty {
            ValType::I32 => Ok(ValType::I32),
            ValType::I64 => Ok(ValType::I64),
            ValType::F32 => Ok(ValType::F32),
            ValType::F64 => Ok(ValType::F64),
            ValType::V128 => Ok(ValType::V128),
            ValType::Ref(ty) => Ok(ValType::Ref(self.arbitrary_super_type_of_ref_type(u, ty)?)),
        }
    }

    fn arbitrary_super_type_of_ref_type(
        &self,
        u: &mut Unstructured,
        ty: RefType,
    ) -> Result<RefType> {
        Ok(RefType {
            // TODO: For now, only create allow nullable reference
            // types. Eventually we should support non-nullable reference types,
            // but this means that we will also need to recognize when it is
            // impossible to create an instance of the reference (eg `(ref
            // nofunc)` has no instances, and self-referential types that
            // contain a non-null self-reference are also impossible to create).
            nullable: true,
            heap_type: self.arbitrary_super_type_of_heap_type(u, ty.heap_type)?,
        })
    }

    fn arbitrary_super_type_of_heap_type(
        &self,
        u: &mut Unstructured,
        ty: HeapType,
    ) -> Result<HeapType> {
        use {AbstractHeapType as AHT, CompositeInnerType as CT, HeapType as HT};

        if !self.config.gc_enabled {
            return Ok(ty);
        }

        let mut choices = vec![ty];
        match ty {
            HT::Abstract { shared, ty } => {
                use AbstractHeapType::*;
                let add_abstract = |choices: &mut Vec<HT>, tys: &[AHT]| {
                    choices.extend(tys.iter().map(|&ty| HT::Abstract { shared, ty }));
                };
                let add_concrete = |choices: &mut Vec<HT>, tys: &[u32]| {
                    choices.extend(
                        tys.iter()
                            .filter(|&&idx| shared == self.is_shared_type(idx))
                            .copied()
                            .map(HT::Concrete),
                    );
                };
                match ty {
                    None => {
                        add_abstract(&mut choices, &[Any, Eq, Struct, Array, I31]);
                        add_concrete(&mut choices, &self.array_types);
                        add_concrete(&mut choices, &self.struct_types);
                    }
                    NoExtern => {
                        add_abstract(&mut choices, &[Extern]);
                    }
                    NoFunc => {
                        add_abstract(&mut choices, &[Func]);
                        add_concrete(&mut choices, &self.func_types);
                    }
                    NoExn => {
                        add_abstract(&mut choices, &[Exn]);
                    }
                    Struct | Array | I31 => {
                        add_abstract(&mut choices, &[Any, Eq]);
                    }
                    Eq => {
                        add_abstract(&mut choices, &[Any]);
                    }
                    NoCont => {
                        add_abstract(&mut choices, &[Cont]);
                    }
                    Exn | Any | Func | Extern | Cont => {}
                }
            }
            HT::Concrete(mut idx) => {
                if let Some(sub_ty) = &self.types.get(usize::try_from(idx).unwrap()) {
                    use AbstractHeapType::*;
                    let ht = |ty| HT::Abstract {
                        shared: sub_ty.composite_type.shared,
                        ty,
                    };
                    match &sub_ty.composite_type.inner {
                        CT::Array(_) => {
                            choices.extend([ht(Any), ht(Eq), ht(Array)]);
                        }
                        CT::Func(_) => {
                            choices.push(ht(Func));
                        }
                        CT::Struct(_) => {
                            choices.extend([ht(Any), ht(Eq), ht(Struct)]);
                        }
                    }
                } else {
                    // Same as in `arbitrary_matching_heap_type`: this was a
                    // forward reference to a concrete type that is part of
                    // this same rec group we are generating right now, and
                    // therefore we haven't generated that type yet. Just
                    // leave `choices` as it is and we will choose the
                    // original type again down below.
                }
                while let Some(supertype) = self
                    .types
                    .get(usize::try_from(idx).unwrap())
                    .and_then(|ty| ty.supertype)
                {
                    choices.push(HT::Concrete(supertype));
                    idx = supertype;
                }
            }
        }
        Ok(*u.choose(&choices)?)
    }

    fn arbitrary_composite_type(&mut self, u: &mut Unstructured) -> Result<CompositeType> {
        use CompositeInnerType as CT;
        let shared = self.arbitrary_shared(u)?;

        if !self.config.gc_enabled {
            return Ok(CompositeType {
                shared,
                inner: CT::Func(self.propagate_shared(shared, |m| m.arbitrary_func_type(u))?),
            });
        }

        match u.int_in_range(0..=2)? {
            0 => Ok(CompositeType {
                shared,
                inner: CT::Array(ArrayType(
                    self.propagate_shared(shared, |m| m.arbitrary_field_type(u))?,
                )),
            }),
            1 => Ok(CompositeType {
                shared,
                inner: CT::Func(self.propagate_shared(shared, |m| m.arbitrary_func_type(u))?),
            }),
            2 => Ok(CompositeType {
                shared,
                inner: CT::Struct(self.propagate_shared(shared, |m| m.arbitrary_struct_type(u))?),
            }),
            _ => unreachable!(),
        }
    }

    fn arbitrary_struct_type(&mut self, u: &mut Unstructured) -> Result<StructType> {
        let len = u.int_in_range(0..=20)?;
        let mut fields = Vec::with_capacity(len);
        for _ in 0..len {
            fields.push(self.arbitrary_field_type(u)?);
        }
        Ok(StructType {
            fields: fields.into_boxed_slice(),
        })
    }

    fn arbitrary_field_type(&mut self, u: &mut Unstructured) -> Result<FieldType> {
        Ok(FieldType {
            element_type: self.arbitrary_storage_type(u)?,
            mutable: u.arbitrary()?,
        })
    }

    fn arbitrary_storage_type(&mut self, u: &mut Unstructured) -> Result<StorageType> {
        match u.int_in_range(0..=2)? {
            0 => Ok(StorageType::I8),
            1 => Ok(StorageType::I16),
            2 => Ok(StorageType::Val(self.arbitrary_valtype(u)?)),
            _ => unreachable!(),
        }
    }

    fn arbitrary_ref_type(&self, u: &mut Unstructured) -> Result<RefType> {
        if !self.config.reference_types_enabled {
            return Ok(RefType::FUNCREF);
        }
        Ok(RefType {
            nullable: true,
            heap_type: self.arbitrary_heap_type(u)?,
        })
    }

    fn arbitrary_heap_type(&self, u: &mut Unstructured) -> Result<HeapType> {
        assert!(self.config.reference_types_enabled);

        let concrete_type_limit = match self.max_type_limit {
            MaxTypeLimit::Num(n) => n,
            MaxTypeLimit::ModuleTypes => u32::try_from(self.types.len()).unwrap(),
        };

        if self.config.gc_enabled && concrete_type_limit > 0 && u.arbitrary()? {
            let idx = u.int_in_range(0..=concrete_type_limit - 1)?;
            // If the caller is demanding a shared heap type but the concrete
            // type we found is not in fact shared, we skip down below to use an
            // abstract heap type instead. If the caller is not demanding a
            // shared type, though, we can use either a shared or unshared
            // concrete type.
            if let Some(ty) = self.types.get(idx as usize) {
                // TODO: in the future, once we can easily query a list of
                // existing shared types, remove this extra check.
                if !(self.must_share && !ty.composite_type.shared) {
                    return Ok(HeapType::Concrete(idx));
                }
            }
        }

        use AbstractHeapType::*;
        let mut choices = vec![Func, Extern];
        if self.config.exceptions_enabled {
            choices.push(Exn);
        }
        if self.config.gc_enabled {
            choices.extend(
                [Any, None, NoExtern, NoFunc, Eq, Struct, Array, I31]
                    .iter()
                    .copied(),
            );
        }

        Ok(HeapType::Abstract {
            shared: self.arbitrary_shared(u)?,
            ty: *u.choose(&choices)?,
        })
    }

    fn arbitrary_func_type(&mut self, u: &mut Unstructured) -> Result<Rc<FuncType>> {
        let mut params = vec![];
        let mut results = vec![];
        let max_params = 20;
        arbitrary_loop(u, 0, max_params, |u| {
            params.push(self.arbitrary_valtype(u)?);
            Ok(true)
        })?;
        let max_results = if self.config.multi_value_enabled {
            max_params
        } else {
            1
        };
        arbitrary_loop(u, 0, max_results, |u| {
            results.push(self.arbitrary_valtype(u)?);
            Ok(true)
        })?;
        Ok(Rc::new(FuncType { params, results }))
    }

    fn can_add_local_or_import_tag(&self) -> bool {
        self.config.exceptions_enabled
            && self.has_tag_func_types()
            && self.tags.len() < self.config.max_tags
    }

    fn can_add_local_or_import_func(&self) -> bool {
        !self.func_types.is_empty() && self.funcs.len() < self.config.max_funcs
    }

    fn can_add_local_or_import_table(&self) -> bool {
        self.tables.len() < self.config.max_tables
    }

    fn can_add_local_or_import_global(&self) -> bool {
        self.globals.len() < self.config.max_globals
    }

    fn can_add_local_or_import_memory(&self) -> bool {
        self.memories.len() < self.config.max_memories
    }

    fn imports_exports_from_module_shape(&mut self, u: &mut Unstructured) -> Result<bool> {
        let example_module = if let Some(wasm) = self.config.module_shape.clone() {
            wasm
        } else {
            return Ok(false);
        };

        #[cfg(feature = "wasmparser")]
        {
            self._imports_exports_from_module_shape(u, &example_module)?;
            Ok(true)
        }
        #[cfg(not(feature = "wasmparser"))]
        {
            let _ = (example_module, u);
            panic!("support for `module_shape` was disabled at compile time");
        }
    }

    #[cfg(feature = "wasmparser")]
    fn _imports_exports_from_module_shape(
        &mut self,
        u: &mut Unstructured,
        example_module: &[u8],
    ) -> Result<()> {
        // First, we validate the module-by-example and extract the required types, imports
        // and exports. Besides, we also extract the functions, tags, tables, memories and
        // globals that are necessary for generating the exports.
        let mut available_funcs: Vec<u32> = Vec::new();
        let mut available_tags: Vec<wasmparser::TagType> = Vec::new();
        let mut available_tables: Vec<wasmparser::TableType> = Vec::new();
        let mut available_globals: Vec<wasmparser::GlobalType> = Vec::new();
        let mut available_memories: Vec<wasmparser::MemoryType> = Vec::new();

        let mut required_types: Vec<SubType> = Vec::new();
        let mut required_recgrps: Vec<usize> = Vec::new();
        let mut required_imports: Vec<wasmparser::Import> = Vec::new();
        let mut required_exports: Vec<wasmparser::Export> = Vec::new();
        let mut validator = wasmparser::Validator::new();
        validator
            .validate_all(example_module)
            .expect("Failed to validate `module_shape` module");
        for payload in wasmparser::Parser::new(0).parse_all(&example_module) {
            match payload.expect("could not parse the `module_shape` module") {
                wasmparser::Payload::TypeSection(type_reader) => {
                    for recgrp in type_reader {
                        let recgrp = recgrp.expect("could not read recursive group");
                        required_recgrps.push(recgrp.types().len());
                        for subtype in recgrp.into_types() {
                            let mut subtype: SubType = subtype.try_into().unwrap();
                            if let Some(supertype_idx) = subtype.supertype {
                                subtype.depth = required_types[supertype_idx as usize].depth + 1;
                            }
                            required_types.push(subtype);
                        }
                    }
                }
                wasmparser::Payload::ImportSection(import_reader) => {
                    for im in import_reader {
                        let im = im.expect("could not read import");
                        required_imports.push(im);
                    }
                }
                wasmparser::Payload::ExportSection(export_reader) => {
                    for ex in export_reader {
                        let ex = ex.expect("could not read export");
                        required_exports.push(ex);
                    }
                }
                wasmparser::Payload::FunctionSection(function_reader) => {
                    for func in function_reader {
                        let func = func.expect("could not read function");
                        available_funcs.push(func);
                    }
                }
                wasmparser::Payload::TagSection(tag_reader) => {
                    for tag in tag_reader {
                        let tag = tag.expect("could not read tag");
                        available_tags.push(tag);
                    }
                }
                wasmparser::Payload::TableSection(table_reader) => {
                    for table in table_reader {
                        let table = table.expect("could not read table");
                        available_tables.push(table.ty);
                    }
                }
                wasmparser::Payload::MemorySection(memory_reader) => {
                    for memory in memory_reader {
                        let memory = memory.expect("could not read memory");
                        available_memories.push(memory);
                    }
                }
                wasmparser::Payload::GlobalSection(global_reader) => {
                    for global in global_reader {
                        let global = global.expect("could not read global");
                        available_globals.push(global.ty);
                    }
                }
                _ => {}
            }
        }

        // Next, we copy all the types from the module-by-example into current module. This is necessary
        // to ensure that the current module has all the types it needs to type-check correctly.
        let mut recgrp_start_idx = self.types.len();
        for size in required_recgrps {
            self.rec_groups
                .push(recgrp_start_idx..recgrp_start_idx + size);
            recgrp_start_idx += size;
        }
        for ty in &required_types {
            self.add_type(ty.clone());
        }

        // We then generate import entries which refer to the imported types. Additionally, we add the
        // imported items to their corresponding vectors here, ensuring that exports reference the
        // correct items.
        let mut imported_funcs: Vec<u32> = Vec::new();
        let mut imported_tags: Vec<wasmparser::TagType> = Vec::new();
        let mut imported_tables: Vec<wasmparser::TableType> = Vec::new();
        let mut imported_globals: Vec<wasmparser::GlobalType> = Vec::new();
        let mut imported_memories: Vec<wasmparser::MemoryType> = Vec::new();
        let mut new_imports = Vec::with_capacity(required_imports.len());
        for import in required_imports {
            let entity_type = match &import.ty {
                wasmparser::TypeRef::Func(sig_idx) => {
                    imported_funcs.push(*sig_idx);
                    match required_types.get(*sig_idx as usize) {
                        None => panic!("signature index refers to a type out of bounds"),
                        Some(ty) => match &ty.composite_type.inner {
                            CompositeInnerType::Func(func_type) => {
                                let entity = EntityType::Func(*sig_idx, Rc::clone(func_type));
                                self.funcs.push((*sig_idx, Rc::clone(func_type)));
                                entity
                            }
                            _ => panic!("a function type is required for function import"),
                        },
                    }
                }

                wasmparser::TypeRef::Tag(wasmparser::TagType {
                    kind,
                    func_type_idx,
                }) => {
                    imported_tags.push(wasmparser::TagType {
                        kind: *kind,
                        func_type_idx: *func_type_idx,
                    });
                    match required_types.get(*func_type_idx as usize) {
                        None => {
                            panic!("function type index for tag refers to a type out of bounds")
                        }
                        Some(ty) => match &ty.composite_type.inner {
                            CompositeInnerType::Func(func_type) => {
                                let tag_type = TagType {
                                    func_type_idx: *func_type_idx,
                                    func_type: Rc::clone(func_type),
                                };
                                let entity = EntityType::Tag(tag_type.clone());
                                self.tags.push(tag_type);
                                entity
                            }
                            _ => panic!("a function type is required for tag import"),
                        },
                    }
                }

                wasmparser::TypeRef::Table(table_ty) => {
                    imported_tables.push(*table_ty);
                    let table_ty = TableType::try_from(*table_ty).unwrap();
                    let entity = EntityType::Table(table_ty);
                    self.tables.push(table_ty);
                    entity
                }

                wasmparser::TypeRef::Memory(memory_ty) => {
                    imported_memories.push(*memory_ty);
                    let memory_ty = MemoryType::from(*memory_ty);
                    let entity = EntityType::Memory(memory_ty);
                    self.memories.push(memory_ty);
                    entity
                }

                wasmparser::TypeRef::Global(global_ty) => {
                    imported_globals.push(*global_ty);
                    let global_ty = GlobalType::try_from(*global_ty).unwrap();
                    let entity = EntityType::Global(global_ty);
                    self.globals.push(global_ty);
                    entity
                }
            };
            new_imports.push(Import {
                module: import.module.to_string(),
                field: import.name.to_string(),
                entity_type,
            });
            self.num_imports += 1;
        }
        self.imports.extend(new_imports);
        available_tags.splice(0..0, imported_tags);
        available_funcs.splice(0..0, imported_funcs);
        available_tables.splice(0..0, imported_tables);
        available_globals.splice(0..0, imported_globals);
        available_memories.splice(0..0, imported_memories);

        // Next, we generate export entries which refer to the export specifications.
        for export in required_exports {
            let index = match export.kind {
                wasmparser::ExternalKind::Func => {
                    match available_funcs.get(export.index as usize) {
                        None => panic!("function index out of bounds"),
                        Some(sig_idx) => match required_types.get(*sig_idx as usize) {
                            None => panic!("signature index refers to a type out of bounds"),
                            Some(ty) => match &ty.composite_type.inner {
                                CompositeInnerType::Func(func_type) => {
                                    let func_index = self.funcs.len() as u32;
                                    self.funcs.push((*sig_idx, Rc::clone(func_type)));
                                    self.num_defined_funcs += 1;
                                    func_index
                                }
                                _ => panic!("a function type is required for function export"),
                            },
                        },
                    }
                }

                wasmparser::ExternalKind::Tag => match available_tags.get(export.index as usize) {
                    None => panic!("tag index out of bounds"),
                    Some(wasmparser::TagType { func_type_idx, .. }) => {
                        match required_types.get(*func_type_idx as usize) {
                            None => {
                                panic!("function type index for tag refers to a type out of bounds")
                            }
                            Some(ty) => match &ty.composite_type.inner {
                                CompositeInnerType::Func(func_type) => {
                                    let tag_index = self.tags.len() as u32;
                                    self.tags.push(TagType {
                                        func_type_idx: *func_type_idx,
                                        func_type: Rc::clone(func_type),
                                    });
                                    self.num_defined_tags += 1;
                                    tag_index
                                }
                                _ => panic!("a function type is required for tag export"),
                            },
                        }
                    }
                },

                wasmparser::ExternalKind::Table => {
                    match available_tables.get(export.index as usize) {
                        None => panic!("table index out of bounds"),
                        Some(ty) => {
                            self.add_arbitrary_table_of_type((*ty).try_into().unwrap(), u)?
                        }
                    }
                }

                wasmparser::ExternalKind::Memory => {
                    match available_memories.get(export.index as usize) {
                        None => panic!("memory index out of bounds"),
                        Some(ty) => self.add_arbitrary_memory_of_type((*ty).into())?,
                    }
                }

                wasmparser::ExternalKind::Global => {
                    match available_globals.get(export.index as usize) {
                        None => panic!("global index out of bounds"),
                        Some(ty) => {
                            self.add_arbitrary_global_of_type((*ty).try_into().unwrap(), u)?
                        }
                    }
                }
            };
            self.exports
                .push((export.name.to_string(), export.kind.into(), index));
            self.export_names.insert(export.name.to_string());
        }

        Ok(())
    }

    fn arbitrary_imports(&mut self, u: &mut Unstructured) -> Result<()> {
        if self.config.max_type_size < self.type_size {
            return Ok(());
        }

        let mut import_strings = HashSet::new();
        let mut choices: Vec<fn(&mut Unstructured, &mut Module) -> Result<EntityType>> =
            Vec::with_capacity(5);
        let min = self.config.min_imports.saturating_sub(self.num_imports);
        let max = self.config.max_imports.saturating_sub(self.num_imports);
        arbitrary_loop(u, min, max, |u| {
            choices.clear();
            if self.can_add_local_or_import_tag() {
                choices.push(|u, m| {
                    let ty = m.arbitrary_tag_type(u)?;
                    Ok(EntityType::Tag(ty))
                });
            }
            if self.can_add_local_or_import_func() {
                choices.push(|u, m| {
                    let idx = *u.choose(&m.func_types)?;
                    let ty = m.func_type(idx).clone();
                    Ok(EntityType::Func(idx, ty))
                });
            }
            if self.can_add_local_or_import_global() {
                choices.push(|u, m| {
                    let ty = m.arbitrary_global_type(u)?;
                    Ok(EntityType::Global(ty))
                });
            }
            if self.can_add_local_or_import_memory() {
                choices.push(|u, m| {
                    let ty = arbitrary_memtype(u, m.config())?;
                    Ok(EntityType::Memory(ty))
                });
            }
            if self.can_add_local_or_import_table() {
                choices.push(|u, m| {
                    let ty = arbitrary_table_type(u, m.config(), Some(m))?;
                    Ok(EntityType::Table(ty))
                });
            }

            if choices.is_empty() {
                // We are out of choices. If we have not have reached the
                // minimum yet, then we have no way to satisfy the constraint,
                // but we follow max-constraints before the min-import
                // constraint.
                return Ok(false);
            }

            // Generate a type to import, but only actually add the item if the
            // type size budget allows us to.
            let f = u.choose(&choices)?;
            let entity_type = f(u, self)?;
            let budget = self.config.max_type_size - self.type_size;
            if entity_type.size() + 1 > budget {
                return Ok(false);
            }
            self.type_size += entity_type.size() + 1;

            // Generate an arbitrary module/name pair to name this import.
            let mut import_pair = unique_import_strings(1_000, u)?;
            if self.duplicate_imports_behavior == DuplicateImportsBehavior::Disallowed {
                while import_strings.contains(&import_pair) {
                    use std::fmt::Write;
                    write!(&mut import_pair.1, "{}", import_strings.len()).unwrap();
                }
                import_strings.insert(import_pair.clone());
            }
            let (module, field) = import_pair;

            // Once our name is determined, then we push the typed item into the
            // appropriate namespace.
            match &entity_type {
                EntityType::Tag(ty) => self.tags.push(ty.clone()),
                EntityType::Func(idx, ty) => self.funcs.push((*idx, ty.clone())),
                EntityType::Global(ty) => self.globals.push(*ty),
                EntityType::Table(ty) => self.tables.push(*ty),
                EntityType::Memory(ty) => self.memories.push(*ty),
            }

            self.num_imports += 1;
            self.imports.push(Import {
                module,
                field,
                entity_type,
            });
            Ok(true)
        })?;

        Ok(())
    }

    /// Generate some arbitrary imports from the list of available imports.
    ///
    /// Returns `true` if there was a list of available imports
    /// configured. Otherwise `false` and the caller should generate arbitrary
    /// imports.
    fn arbitrary_imports_from_available(&mut self, u: &mut Unstructured) -> Result<bool> {
        let example_module = if let Some(wasm) = self.config.available_imports.take() {
            wasm
        } else {
            return Ok(false);
        };

        #[cfg(feature = "wasmparser")]
        {
            self._arbitrary_imports_from_available(u, &example_module)?;
            Ok(true)
        }
        #[cfg(not(feature = "wasmparser"))]
        {
            let _ = (example_module, u);
            panic!("support for `available_imports` was disabled at compile time");
        }
    }

    #[cfg(feature = "wasmparser")]
    fn _arbitrary_imports_from_available(
        &mut self,
        u: &mut Unstructured,
        example_module: &[u8],
    ) -> Result<()> {
        // First, parse the module-by-example to collect the types and imports.
        //
        // `available_types` will map from a signature index (which is the same as the index into
        // this vector) as it appears in the parsed code, to the type itself. We copy all the types
        // from module-by-example into the module being constructed for the sake of simplicity
        // and for this reason, [`Self::config::max_types`] may be surpassed.
        let mut new_recgrps = Vec::<usize>::new();
        let mut available_types = Vec::<SubType>::new();
        let mut available_imports = Vec::<wasmparser::Import>::new();
        let mut validator = wasmparser::Validator::new();
        validator
            .validate_all(example_module)
            .expect("Failed to validate `module_shape` module");
        for payload in wasmparser::Parser::new(0).parse_all(&example_module) {
            match payload.expect("could not parse the available import payload") {
                wasmparser::Payload::TypeSection(type_reader) => {
                    for recgrp in type_reader {
                        let recgrp = recgrp.expect("could not read recursive group");
                        new_recgrps.push(recgrp.types().len());
                        for subtype in recgrp.into_types() {
                            let mut subtype: SubType = subtype.try_into().unwrap();
                            if let Some(supertype_idx) = subtype.supertype {
                                subtype.depth = available_types[supertype_idx as usize].depth + 1;
                            }
                            available_types.push(subtype);
                        }
                    }
                }
                wasmparser::Payload::ImportSection(import_reader) => {
                    for im in import_reader {
                        let im = im.expect("could not read import");
                        // We can immediately filter whether this is an import we want to
                        // use.
                        let use_import = u.arbitrary().unwrap_or(false);
                        if !use_import {
                            continue;
                        }
                        available_imports.push(im);
                    }
                }
                _ => {}
            }
        }

        // We then generate import entries which refer to the imported types. Since this function
        // is called at the very beginning of the module generation process and all types from the
        // module-by-example are copied into the current module, no further adjustments are needed
        // for type indices.
        let mut new_imports = Vec::with_capacity(available_imports.len());
        for import in available_imports {
            let type_size_budget = self.config.max_type_size - self.type_size;
            let entity_type = match &import.ty {
                wasmparser::TypeRef::Func(sig_idx) => {
                    if self.funcs.len() >= self.config.max_funcs {
                        continue;
                    } else {
                        match available_types.get(*sig_idx as usize) {
                            None => panic!("signature index refers to a type out of bounds"),
                            Some(ty) => match &ty.composite_type.inner {
                                CompositeInnerType::Func(func_type) => {
                                    let entity = EntityType::Func(*sig_idx, Rc::clone(func_type));
                                    if type_size_budget < entity.size() {
                                        continue;
                                    }
                                    self.funcs.push((*sig_idx, Rc::clone(func_type)));
                                    entity
                                }
                                _ => panic!("a function type is required for function import"),
                            },
                        }
                    }
                }

                wasmparser::TypeRef::Tag(wasmparser::TagType { func_type_idx, .. }) => {
                    let can_add_tag = self.tags.len() < self.config.max_tags;
                    if !self.config.exceptions_enabled || !can_add_tag {
                        continue;
                    } else {
                        match available_types.get(*func_type_idx as usize) {
                            None => {
                                panic!("function type index for tag refers to a type out of bounds")
                            }
                            Some(ty) => match &ty.composite_type.inner {
                                CompositeInnerType::Func(func_type) => {
                                    let tag_type = TagType {
                                        func_type_idx: *func_type_idx,
                                        func_type: Rc::clone(func_type),
                                    };
                                    let entity = EntityType::Tag(tag_type.clone());
                                    if type_size_budget < entity.size() {
                                        continue;
                                    }
                                    self.tags.push(tag_type);
                                    entity
                                }
                                _ => panic!("a function type is required for tag import"),
                            },
                        }
                    }
                }

                wasmparser::TypeRef::Table(table_ty) => {
                    let table_ty = TableType::try_from(*table_ty).unwrap();
                    let entity = EntityType::Table(table_ty);
                    let type_size = entity.size();
                    if type_size_budget < type_size || !self.can_add_local_or_import_table() {
                        continue;
                    }
                    self.type_size += type_size;
                    self.tables.push(table_ty);
                    entity
                }

                wasmparser::TypeRef::Memory(memory_ty) => {
                    let memory_ty = MemoryType::from(*memory_ty);
                    let entity = EntityType::Memory(memory_ty);
                    let type_size = entity.size();
                    if type_size_budget < type_size || !self.can_add_local_or_import_memory() {
                        continue;
                    }
                    self.type_size += type_size;
                    self.memories.push(memory_ty);
                    entity
                }

                wasmparser::TypeRef::Global(global_ty) => {
                    let global_ty = GlobalType::try_from(*global_ty).unwrap();
                    let entity = EntityType::Global(global_ty);
                    let type_size = entity.size();
                    if type_size_budget < type_size || !self.can_add_local_or_import_global() {
                        continue;
                    }
                    self.type_size += type_size;
                    self.globals.push(global_ty);
                    entity
                }
            };
            new_imports.push(Import {
                module: import.module.to_string(),
                field: import.name.to_string(),
                entity_type,
            });
            self.num_imports += 1;
        }

        // Finally, add the entities we just generated.
        let mut recgrp_start_idx = self.types.len();
        for size in new_recgrps {
            self.rec_groups
                .push(recgrp_start_idx..recgrp_start_idx + size);
            recgrp_start_idx += size;
        }
        for ty in available_types {
            self.add_type(ty);
        }
        self.imports.extend(new_imports);

        Ok(())
    }

    fn type_of(&self, kind: ExportKind, index: u32) -> EntityType {
        match kind {
            ExportKind::Global => EntityType::Global(self.globals[index as usize]),
            ExportKind::Memory => EntityType::Memory(self.memories[index as usize]),
            ExportKind::Table => EntityType::Table(self.tables[index as usize]),
            ExportKind::Func => {
                let (_idx, ty) = &self.funcs[index as usize];
                EntityType::Func(u32::max_value(), ty.clone())
            }
            ExportKind::Tag => EntityType::Tag(self.tags[index as usize].clone()),
        }
    }

    fn ty(&self, idx: u32) -> &SubType {
        &self.types[idx as usize]
    }

    fn func_types(&self) -> impl Iterator<Item = (u32, &FuncType)> + '_ {
        self.func_types
            .iter()
            .copied()
            .map(move |type_i| (type_i, &**self.func_type(type_i)))
    }

    fn func_type(&self, idx: u32) -> &Rc<FuncType> {
        match &self.ty(idx).composite_type.inner {
            CompositeInnerType::Func(f) => f,
            _ => panic!("types[{idx}] is not a func type"),
        }
    }

    fn tags(&self) -> impl Iterator<Item = (u32, &TagType)> + '_ {
        self.tags
            .iter()
            .enumerate()
            .map(move |(i, ty)| (i as u32, ty))
    }

    fn funcs(&self) -> impl Iterator<Item = (u32, &Rc<FuncType>)> + '_ {
        self.funcs
            .iter()
            .enumerate()
            .map(move |(i, (_, ty))| (i as u32, ty))
    }

    fn has_tag_func_types(&self) -> bool {
        self.tag_func_types().next().is_some()
    }

    fn tag_func_types(&self) -> impl Iterator<Item = u32> + '_ {
        self.func_types
            .iter()
            .copied()
            .filter(move |i| self.func_type(*i).results.is_empty())
    }

    fn arbitrary_valtype(&self, u: &mut Unstructured) -> Result<ValType> {
        #[derive(PartialEq, Eq, PartialOrd, Ord)]
        enum ValTypeClass {
            I32,
            I64,
            F32,
            F64,
            V128,
            Ref,
        }

        let mut val_classes: Vec<_> = self
            .valtypes
            .iter()
            .map(|vt| match vt {
                ValType::I32 => ValTypeClass::I32,
                ValType::I64 => ValTypeClass::I64,
                ValType::F32 => ValTypeClass::F32,
                ValType::F64 => ValTypeClass::F64,
                ValType::V128 => ValTypeClass::V128,
                ValType::Ref(_) => ValTypeClass::Ref,
            })
            .collect();
        val_classes.sort_unstable();
        val_classes.dedup();

        match u.choose(&val_classes)? {
            ValTypeClass::I32 => Ok(ValType::I32),
            ValTypeClass::I64 => Ok(ValType::I64),
            ValTypeClass::F32 => Ok(ValType::F32),
            ValTypeClass::F64 => Ok(ValType::F64),
            ValTypeClass::V128 => Ok(ValType::V128),
            ValTypeClass::Ref => Ok(ValType::Ref(self.arbitrary_ref_type(u)?)),
        }
    }

    fn arbitrary_global_type(&self, u: &mut Unstructured) -> Result<GlobalType> {
        let val_type = self.arbitrary_valtype(u)?;
        // Propagate the inner type's sharedness to the global type.
        let shared = match val_type {
            ValType::I32 | ValType::I64 | ValType::F32 | ValType::F64 | ValType::V128 => {
                self.arbitrary_shared(u)?
            }
            ValType::Ref(r) => self.is_shared_ref_type(r),
        };
        Ok(GlobalType {
            val_type,
            mutable: u.arbitrary()?,
            shared,
        })
    }

    fn arbitrary_tag_type(&self, u: &mut Unstructured) -> Result<TagType> {
        let candidate_func_types: Vec<_> = self.tag_func_types().collect();
        arbitrary_tag_type(u, &candidate_func_types, |ty_idx| {
            self.func_type(ty_idx).clone()
        })
    }

    fn arbitrary_tags(&mut self, u: &mut Unstructured) -> Result<()> {
        if !self.config.exceptions_enabled || !self.has_tag_func_types() {
            return Ok(());
        }

        arbitrary_loop(u, self.config.min_tags, self.config.max_tags, |u| {
            if !self.can_add_local_or_import_tag() {
                return Ok(false);
            }
            self.tags.push(self.arbitrary_tag_type(u)?);
            self.num_defined_tags += 1;
            Ok(true)
        })
    }

    fn arbitrary_funcs(&mut self, u: &mut Unstructured) -> Result<()> {
        if self.func_types.is_empty() {
            return Ok(());
        }

        // For now, only define non-shared functions. Until we can update
        // instruction generation to understand the additional sharedness
        // validation, we don't want to generate instructions that touch
        // unshared objects from a shared context (TODO: handle shared).
        let unshared_func_types: Vec<_> = self
            .func_types
            .iter()
            .copied()
            .filter(|&i| !self.is_shared_type(i))
            .collect();
        if unshared_func_types.is_empty() {
            return Ok(());
        }

        arbitrary_loop(u, self.config.min_funcs, self.config.max_funcs, |u| {
            if !self.can_add_local_or_import_func() {
                return Ok(false);
            }
            let max = unshared_func_types.len() - 1;
            let ty = unshared_func_types[u.int_in_range(0..=max)?];
            self.funcs.push((ty, self.func_type(ty).clone()));
            self.num_defined_funcs += 1;
            Ok(true)
        })
    }

    fn arbitrary_tables(&mut self, u: &mut Unstructured) -> Result<()> {
        arbitrary_loop(
            u,
            self.config.min_tables as usize,
            self.config.max_tables,
            |u| {
                if !self.can_add_local_or_import_table() {
                    return Ok(false);
                }
                let ty = arbitrary_table_type(u, self.config(), Some(self))?;
                self.add_arbitrary_table_of_type(ty, u)?;
                Ok(true)
            },
        )
    }

    /// Generates an arbitrary table initialization expression for a table whose
    /// element type is `ty`.
    ///
    /// Table initialization expressions were added by the GC proposal to
    /// initialize non-nullable tables.
    fn arbitrary_table_init(
        &mut self,
        u: &mut Unstructured,
        ty: RefType,
    ) -> Result<Option<ConstExpr>> {
        if !self.config.gc_enabled {
            assert!(ty.nullable);
            return Ok(None);
        }
        // Even with the GC proposal an initialization expression is not
        // required if the element type is nullable.
        if ty.nullable && u.arbitrary()? {
            return Ok(None);
        }
        // Only imported globals are allowed in the constant initialization
        // expressions for tables.
        let expr = self.arbitrary_const_expr(ValType::Ref(ty), u, false)?;
        Ok(Some(expr))
    }

    fn arbitrary_memories(&mut self, u: &mut Unstructured) -> Result<()> {
        arbitrary_loop(
            u,
            self.config.min_memories as usize,
            self.config.max_memories,
            |u| {
                if !self.can_add_local_or_import_memory() {
                    return Ok(false);
                }
                let ty = arbitrary_memtype(u, self.config())?;
                self.add_arbitrary_memory_of_type(ty)?;
                Ok(true)
            },
        )
    }

    /// Add a new global of the given type and return its global index.
    fn add_arbitrary_global_of_type(
        &mut self,
        ty: GlobalType,
        u: &mut Unstructured,
    ) -> Result<u32> {
        let expr = self.arbitrary_const_expr(ty.val_type, u, true)?;
        let global_idx = self.globals.len() as u32;
        self.globals.push(ty);
        self.defined_globals.push((global_idx, expr));
        Ok(global_idx)
    }

    /// Add a new memory of the given type and return its memory index.
    fn add_arbitrary_memory_of_type(&mut self, ty: MemoryType) -> Result<u32> {
        let memory_idx = self.memories.len() as u32;
        self.num_defined_memories += 1;
        self.memories.push(ty);
        Ok(memory_idx)
    }

    /// Add a new table of the given type and return its table index.
    fn add_arbitrary_table_of_type(&mut self, ty: TableType, u: &mut Unstructured) -> Result<u32> {
        let expr = self.arbitrary_table_init(u, ty.element_type)?;
        let table_idx = self.tables.len() as u32;
        self.tables.push(ty);
        self.defined_tables.push(expr);
        Ok(table_idx)
    }

    /// Generates an arbitrary constant expression of the type `ty`.
    fn arbitrary_const_expr(
        &mut self,
        ty: ValType,
        u: &mut Unstructured,
        allow_defined_globals: bool,
    ) -> Result<ConstExpr> {
        let mut choices = mem::take(&mut self.const_expr_choices);
        choices.clear();

        // MVP wasm can `global.get` any immutable imported global in a
        // constant expression, and the GC proposal enables this for all
        // globals, so make all matching globals a candidate.
        for i in self.globals_for_const_expr(ty, allow_defined_globals) {
            choices.push(Box::new(move |_, _| Ok(ConstExpr::global_get(i))));
        }

        // Another option for all types is to have an actual value of each type.
        // Change `ty` to any valid subtype of `ty` and then generate a matching
        // type of that value.
        let ty = self.arbitrary_matching_val_type(u, ty)?;
        match ty {
            ValType::I32 => {
                choices.push(Box::new(|u, _| Ok(ConstExpr::i32_const(u.arbitrary()?))));
                if self.config.extended_const_enabled {
                    choices.push(Box::new(arbitrary_extended_const));
                }
            }
            ValType::I64 => {
                choices.push(Box::new(|u, _| Ok(ConstExpr::i64_const(u.arbitrary()?))));
                if self.config.extended_const_enabled {
                    choices.push(Box::new(arbitrary_extended_const));
                }
            }
            ValType::F32 => choices.push(Box::new(|u, _| {
                Ok(ConstExpr::f32_const(u.arbitrary::<f32>()?.into()))
            })),
            ValType::F64 => choices.push(Box::new(|u, _| {
                Ok(ConstExpr::f64_const(u.arbitrary::<f64>()?.into()))
            })),
            ValType::V128 => {
                choices.push(Box::new(|u, _| Ok(ConstExpr::v128_const(u.arbitrary()?))))
            }

            ValType::Ref(ty) => {
                if ty.nullable {
                    choices.push(Box::new(move |_, _| Ok(ConstExpr::ref_null(ty.heap_type))));
                }

                match ty.heap_type {
                    HeapType::Abstract {
                        ty: AbstractHeapType::Func,
                        shared,
                    } => {
                        let num_funcs = self
                            .funcs
                            .iter()
                            .filter(|(t, _)| shared == self.is_shared_type(*t))
                            .count();
                        if num_funcs > 0 {
                            let pick = u.int_in_range(0..=num_funcs - 1)?;
                            let (i, _) = self
                                .funcs
                                .iter()
                                .map(|(t, _)| *t)
                                .enumerate()
                                .filter(|(_, t)| shared == self.is_shared_type(*t))
                                .nth(pick)
                                .unwrap();
                            choices.push(Box::new(move |_, _| Ok(ConstExpr::ref_func(i as u32))));
                        }
                    }

                    HeapType::Concrete(ty) => {
                        for (i, fty) in self.funcs.iter().map(|(t, _)| *t).enumerate() {
                            if ty != fty {
                                continue;
                            }
                            choices.push(Box::new(move |_, _| Ok(ConstExpr::ref_func(i as u32))));
                        }
                    }

                    // TODO: fill out more GC types e.g `array.new` and
                    // `struct.new`
                    _ => {}
                }
            }
        }

        let f = u.choose(&choices)?;
        let ret = f(u, ty);
        self.const_expr_choices = choices;
        return ret;

        /// Implementation of generation of expressions from the
        /// `extended-const` proposal to WebAssembly. This proposal enabled
        /// using `i{32,64}.{add,sub,mul}` in constant expressions in addition
        /// to the previous `i{32,64}.const` instructions. Note that at this
        /// time this doesn't use the full expression generator in
        /// `code_builder.rs` but instead inlines just what's necessary for
        /// constant expressions here.
        fn arbitrary_extended_const(u: &mut Unstructured<'_>, ty: ValType) -> Result<ConstExpr> {
            use wasm_encoder::Instruction::*;

            // This only works for i32/i64, would need refactoring for different
            // types.
            assert!(ty == ValType::I32 || ty == ValType::I64);
            let add = if ty == ValType::I32 { I32Add } else { I64Add };
            let sub = if ty == ValType::I32 { I32Sub } else { I64Sub };
            let mul = if ty == ValType::I32 { I32Mul } else { I64Mul };
            let const_: fn(&mut Unstructured<'_>) -> Result<wasm_encoder::Instruction<'static>> =
                if ty == ValType::I32 {
                    |u| u.arbitrary().map(I32Const)
                } else {
                    |u| u.arbitrary().map(I64Const)
                };

            // Here `instrs` is the list of instructions, in reverse order, that
            // are going to be emitted. The `needed` value keeps track of how
            // many values are needed to complete this expression. New
            // instructions must be generated while some more items are needed.
            let mut instrs = Vec::new();
            let mut needed = 1;
            while needed > 0 {
                // If fuzz data has been exhausted or if this is a "large
                // enough" constant expression then force generation of
                // constants to finish out the expression.
                let choice = if u.is_empty() || instrs.len() > 10 {
                    0
                } else {
                    u.int_in_range(0..=3)?
                };
                match choice {
                    0 => {
                        instrs.push(const_(u)?);
                        needed -= 1;
                    }
                    1 => {
                        instrs.push(add.clone());
                        needed += 1;
                    }
                    2 => {
                        instrs.push(sub.clone());
                        needed += 1;
                    }
                    3 => {
                        instrs.push(mul.clone());
                        needed += 1;
                    }
                    _ => unreachable!(),
                }
            }
            Ok(ConstExpr::extended(instrs.into_iter().rev()))
        }
    }

    fn arbitrary_globals(&mut self, u: &mut Unstructured) -> Result<()> {
        arbitrary_loop(u, self.config.min_globals, self.config.max_globals, |u| {
            if !self.can_add_local_or_import_global() {
                return Ok(false);
            }

            let ty = self.arbitrary_global_type(u)?;
            self.add_arbitrary_global_of_type(ty, u)?;

            Ok(true)
        })
    }

    fn required_exports(&mut self, u: &mut Unstructured) -> Result<bool> {
        let example_module = if let Some(wasm) = self.config.exports.clone() {
            wasm
        } else {
            return Ok(false);
        };

        #[cfg(feature = "wasmparser")]
        {
            self._required_exports(u, &example_module)?;
            Ok(true)
        }
        #[cfg(not(feature = "wasmparser"))]
        {
            let _ = (example_module, u);
            panic!("support for `exports` was disabled at compile time");
        }
    }

    #[cfg(feature = "wasmparser")]
    fn _required_exports(&mut self, u: &mut Unstructured, example_module: &[u8]) -> Result<()> {
        let mut required_exports: Vec<wasmparser::Export> = vec![];
        let mut validator = wasmparser::Validator::new();
        let exports_types = validator
            .validate_all(&example_module)
            .expect("Failed to validate `exports` Wasm");
        for payload in wasmparser::Parser::new(0).parse_all(&example_module) {
            match payload.expect("Failed to read `exports` Wasm") {
                wasmparser::Payload::ExportSection(export_reader) => {
                    required_exports = export_reader
                        .into_iter()
                        .collect::<Result<_, _>>()
                        .expect("Failed to read `exports` export section");
                }
                _ => {}
            }
        }

        // For each export, add necessary prerequisites to the module.
        let exports_types = exports_types.as_ref();
        let check_and_get_func_type =
            |id: wasmparser::types::CoreTypeId| -> (Rc<FuncType>, SubType) {
                let subtype = exports_types.get(id).unwrap_or_else(|| {
                    panic!("Unable to get subtype for {id:?} in `exports` Wasm")
                });
                match &subtype.composite_type.inner {
                    wasmparser::CompositeInnerType::Func(func_type) => {
                        assert!(
                            subtype.is_final,
                            "Subtype {subtype:?} from `exports` Wasm is not final"
                        );
                        assert!(
                            subtype.supertype_idx.is_none(),
                            "Subtype {subtype:?} from `exports` Wasm has non-empty supertype"
                        );
                        let func_type = Rc::new(FuncType {
                            params: func_type
                                .params()
                                .iter()
                                .copied()
                                .map(|t| t.try_into().unwrap())
                                .collect(),
                            results: func_type
                                .results()
                                .iter()
                                .copied()
                                .map(|t| t.try_into().unwrap())
                                .collect(),
                        });
                        let subtype = SubType {
                            is_final: true,
                            supertype: None,
                            depth: 1,
                            composite_type: CompositeType::new_func(
                                Rc::clone(&func_type),
                                subtype.composite_type.shared,
                            ),
                        };
                        (func_type, subtype)
                    }
                    _ => panic!(
                        "Unable to handle type {:?} from `exports` Wasm",
                        subtype.composite_type
                    ),
                }
            };
        for export in required_exports {
            let new_index = match exports_types
                .entity_type_from_export(&export)
                .unwrap_or_else(|| {
                    panic!("Unable to get type from export {export:?} in `exports` Wasm",)
                }) {
                // For functions, add the type and a function with that type.
                wasmparser::types::EntityType::Func(id) => {
                    let (func_type, subtype) = check_and_get_func_type(id);
                    self.rec_groups.push(self.types.len()..self.types.len() + 1);
                    let type_index = self.add_type(subtype);
                    let func_index = self.funcs.len() as u32;
                    self.funcs.push((type_index, func_type));
                    self.num_defined_funcs += 1;
                    func_index
                }
                // For globals, add a new global.
                wasmparser::types::EntityType::Global(global_type) => {
                    self.add_arbitrary_global_of_type(global_type.try_into().unwrap(), u)?
                }
                // For memories, add a new memory.
                wasmparser::types::EntityType::Memory(memory_type) => {
                    self.add_arbitrary_memory_of_type(memory_type.into())?
                }
                // For tables, add a new table.
                wasmparser::types::EntityType::Table(table_type) => {
                    self.add_arbitrary_table_of_type(table_type.try_into().unwrap(), u)?
                }
                // For tags, add the type.
                wasmparser::types::EntityType::Tag(id) => {
                    let (func_type, subtype) = check_and_get_func_type(id);
                    self.rec_groups.push(self.types.len()..self.types.len() + 1);
                    let type_index = self.add_type(subtype);
                    let tag_index = self.tags.len() as u32;
                    self.tags.push(TagType {
                        func_type_idx: type_index,
                        func_type: func_type,
                    });
                    self.num_defined_tags += 1;
                    tag_index
                }
            };
            self.exports
                .push((export.name.to_string(), export.kind.into(), new_index));
            self.export_names.insert(export.name.to_string());
        }

        Ok(())
    }

    fn arbitrary_exports(&mut self, u: &mut Unstructured) -> Result<()> {
        if self.config.max_type_size < self.type_size && !self.config.export_everything {
            return Ok(());
        }

        // Build up a list of candidates for each class of import
        let mut choices: Vec<Vec<(ExportKind, u32)>> = Vec::with_capacity(6);
        choices.push(
            (0..self.funcs.len())
                .map(|i| (ExportKind::Func, i as u32))
                .collect(),
        );
        choices.push(
            (0..self.tables.len())
                .map(|i| (ExportKind::Table, i as u32))
                .collect(),
        );
        choices.push(
            (0..self.memories.len())
                .map(|i| (ExportKind::Memory, i as u32))
                .collect(),
        );
        choices.push(
            (0..self.globals.len())
                .map(|i| (ExportKind::Global, i as u32))
                .collect(),
        );

        // If the configuration demands exporting everything, we do so here and
        // early-return.
        if self.config.export_everything {
            for choices_by_kind in choices {
                for (kind, idx) in choices_by_kind {
                    let name = unique_string(1_000, &mut self.export_names, u)?;
                    self.add_arbitrary_export(name, kind, idx)?;
                }
            }
            return Ok(());
        }

        arbitrary_loop(u, self.config.min_exports, self.config.max_exports, |u| {
            // Remove all candidates for export whose type size exceeds our
            // remaining budget for type size. Then also remove any classes
            // of exports which no longer have any candidates.
            //
            // If there's nothing remaining after this, then we're done.
            let max_size = self.config.max_type_size - self.type_size;
            for list in choices.iter_mut() {
                list.retain(|(kind, idx)| self.type_of(*kind, *idx).size() + 1 < max_size);
            }
            choices.retain(|list| !list.is_empty());
            if choices.is_empty() {
                return Ok(false);
            }

            // Pick a name, then pick the export, and then we can record
            // information about the chosen export.
            let name = unique_string(1_000, &mut self.export_names, u)?;
            let list = u.choose(&choices)?;
            let (kind, idx) = *u.choose(list)?;
            self.add_arbitrary_export(name, kind, idx)?;
            Ok(true)
        })
    }

    fn add_arbitrary_export(&mut self, name: String, kind: ExportKind, idx: u32) -> Result<()> {
        let ty = self.type_of(kind, idx);
        self.type_size += 1 + ty.size();
        if self.type_size <= self.config.max_type_size {
            self.exports.push((name, kind, idx));
            Ok(())
        } else {
            // If our addition of exports takes us above the allowed number of
            // types, we fail; this error code is not the most illustrative of
            // the cause but is the best available from `arbitrary`.
            Err(arbitrary::Error::IncorrectFormat)
        }
    }

    fn arbitrary_start(&mut self, u: &mut Unstructured) -> Result<()> {
        if !self.config.allow_start_export {
            return Ok(());
        }

        let mut choices = Vec::with_capacity(self.funcs.len());

        for (func_idx, ty) in self.funcs() {
            if ty.params.is_empty() && ty.results.is_empty() {
                choices.push(func_idx);
            }
        }

        if !choices.is_empty() && u.arbitrary().unwrap_or(false) {
            let f = *u.choose(&choices)?;
            self.start = Some(f);
        }

        Ok(())
    }

    fn arbitrary_elems(&mut self, u: &mut Unstructured) -> Result<()> {
        // Create a helper closure to choose an arbitrary offset.
        let mut global_i32 = vec![];
        let mut global_i64 = vec![];
        if !self.config.disallow_traps {
            for i in self.globals_for_const_expr(ValType::I32, true) {
                global_i32.push(i);
            }
            for i in self.globals_for_const_expr(ValType::I64, true) {
                global_i64.push(i);
            }
        }
        let disallow_traps = self.config.disallow_traps;
        let arbitrary_active_elem =
            |u: &mut Unstructured, min_mem_size: u64, table: Option<u32>, table_ty: &TableType| {
                let global_choices = if table_ty.table64 {
                    &global_i64
                } else {
                    &global_i32
                };
                let (offset, max_size_hint) = if !global_choices.is_empty() && u.arbitrary()? {
                    let g = u.choose(&global_choices)?;
                    (Offset::Global(*g), None)
                } else {
                    let max_mem_size = if disallow_traps {
                        table_ty.minimum
                    } else if table_ty.table64 {
                        u64::MAX
                    } else {
                        u64::from(u32::MAX)
                    };
                    let offset = arbitrary_offset(u, min_mem_size, max_mem_size, 0)?;
                    let max_size_hint = if disallow_traps
                        || (offset <= min_mem_size
                            && u.int_in_range(0..=CHANCE_OFFSET_INBOUNDS)? != 0)
                    {
                        Some(min_mem_size - offset)
                    } else {
                        None
                    };

                    let offset = if table_ty.table64 {
                        Offset::Const64(offset as i64)
                    } else {
                        Offset::Const32(offset as i32)
                    };
                    (offset, max_size_hint)
                };
                Ok((ElementKind::Active { table, offset }, max_size_hint))
            };

        // Generate a list of candidates for "kinds" of elements segments. For
        // example we can have an active segment for any existing table or
        // passive/declared segments if the right wasm features are enabled.
        type GenElemSegment<'a> =
            dyn Fn(&mut Unstructured) -> Result<(ElementKind, Option<u64>)> + 'a;
        let mut choices: Vec<Box<GenElemSegment>> = Vec::new();

        // Bulk memory enables passive/declared segments, and note that the
        // types used are selected later.
        if self.config.bulk_memory_enabled {
            choices.push(Box::new(|_| Ok((ElementKind::Passive, None))));
            choices.push(Box::new(|_| Ok((ElementKind::Declared, None))));
        }

        for (i, ty) in self.tables.iter().enumerate() {
            // If this table starts with no capacity then any non-empty element
            // segment placed onto it will immediately trap, which isn't too
            // too interesting. If that's the case give it an unlikely chance
            // of proceeding.
            if ty.minimum == 0 && u.int_in_range(0..=CHANCE_SEGMENT_ON_EMPTY)? != 0 {
                continue;
            }

            let minimum = ty.minimum;
            // If the first table is a funcref table then it's a candidate for
            // the MVP encoding of element segments.
            let ty = *ty;
            if i == 0 && ty.element_type == RefType::FUNCREF {
                choices.push(Box::new(move |u| {
                    arbitrary_active_elem(u, minimum, None, &ty)
                }));
            }
            if self.config.bulk_memory_enabled {
                let idx = Some(i as u32);
                choices.push(Box::new(move |u| {
                    arbitrary_active_elem(u, minimum, idx, &ty)
                }));
            }
        }

        if choices.is_empty() {
            return Ok(());
        }

        arbitrary_loop(
            u,
            self.config.min_element_segments,
            self.config.max_element_segments,
            |u| {
                // Pick a kind of element segment to generate which will also
                // give us a hint of the maximum size, if any.
                let (kind, max_size_hint) = u.choose(&choices)?(u)?;
                let max = max_size_hint
                    .map(|i| usize::try_from(i).unwrap())
                    .unwrap_or_else(|| self.config.max_elements);

                // Infer, from the kind of segment, the type of the element
                // segment. Passive/declared segments can be declared with any
                // reference type, but active segments must match their table.
                let ty = match kind {
                    ElementKind::Passive | ElementKind::Declared => self.arbitrary_ref_type(u)?,
                    ElementKind::Active { table, .. } => {
                        let idx = table.unwrap_or(0);
                        self.arbitrary_matching_ref_type(u, self.tables[idx as usize].element_type)?
                    }
                };

                // The `Elements::Functions` encoding is only possible when the
                // element type is a `funcref` because the binary format can't
                // allow encoding any other type in that form.
                let can_use_function_list = ty == RefType::FUNCREF;
                if !self.config.reference_types_enabled {
                    assert!(can_use_function_list);
                }

                // If a function list is possible then build up a list of
                // functions that can be selected from.
                let mut func_candidates = Vec::new();
                if can_use_function_list {
                    match ty.heap_type {
                        HeapType::Abstract {
                            ty: AbstractHeapType::Func,
                            ..
                        } => {
                            func_candidates.extend(0..self.funcs.len() as u32);
                        }
                        HeapType::Concrete(ty) => {
                            for (i, (fty, _)) in self.funcs.iter().enumerate() {
                                if *fty == ty {
                                    func_candidates.push(i as u32);
                                }
                            }
                        }
                        _ => {}
                    }
                }

                // And finally actually generate the arbitrary elements of this
                // element segment. Function indices are used if they're either
                // forced or allowed, and otherwise expressions are used
                // instead.
                let items = if !self.config.reference_types_enabled
                    || (can_use_function_list && u.arbitrary()?)
                {
                    let mut init = vec![];
                    if func_candidates.len() > 0 {
                        arbitrary_loop(u, self.config.min_elements, max, |u| {
                            let func_idx = *u.choose(&func_candidates)?;
                            init.push(func_idx);
                            Ok(true)
                        })?;
                    }
                    Elements::Functions(init)
                } else {
                    let mut init = vec![];
                    arbitrary_loop(u, self.config.min_elements, max, |u| {
                        init.push(self.arbitrary_const_expr(ValType::Ref(ty), u, true)?);
                        Ok(true)
                    })?;
                    Elements::Expressions(init)
                };

                self.elems.push(ElementSegment { kind, ty, items });
                Ok(true)
            },
        )
    }

    fn arbitrary_code(&mut self, u: &mut Unstructured) -> Result<()> {
        self.compute_interesting_values();

        self.code.reserve(self.num_defined_funcs);
        let mut allocs = CodeBuilderAllocations::new(
            self,
            self.config.exports.is_some() || self.config.module_shape.is_some(),
        );
        for (idx, ty) in self.funcs[self.funcs.len() - self.num_defined_funcs..].iter() {
            let shared = self.is_shared_type(*idx);
            let body = self.arbitrary_func_body(u, ty, &mut allocs, shared)?;
            self.code.push(body);
        }
        allocs.finish(u, self)?;
        Ok(())
    }

    fn arbitrary_func_body(
        &self,
        u: &mut Unstructured,
        ty: &FuncType,
        allocs: &mut CodeBuilderAllocations,
        shared: bool,
    ) -> Result<Code> {
        let mut locals = self.arbitrary_locals(u)?;
        let builder = allocs.builder(ty, &mut locals, shared);
        let instructions = if self.config.allow_invalid_funcs && u.arbitrary().unwrap_or(false) {
            Instructions::Arbitrary(arbitrary_vec_u8(u)?)
        } else {
            Instructions::Generated(builder.arbitrary(u, self)?)
        };

        Ok(Code {
            locals,
            instructions,
        })
    }

    fn arbitrary_locals(&self, u: &mut Unstructured) -> Result<Vec<ValType>> {
        let mut ret = Vec::new();
        arbitrary_loop(u, 0, 100, |u| {
            ret.push(self.arbitrary_valtype(u)?);
            Ok(true)
        })?;
        Ok(ret)
    }

    fn arbitrary_data(&mut self, u: &mut Unstructured) -> Result<()> {
        // With bulk-memory we can generate passive data, otherwise if there are
        // no memories we can't generate any data.
        let memories = self.memories.len() as u32;
        if memories == 0 && !self.config.bulk_memory_enabled {
            return Ok(());
        }
        let disallow_traps = self.config.disallow_traps;
        let mut choices32: Vec<Box<dyn Fn(&mut Unstructured, u64, usize) -> Result<Offset>>> =
            vec![];
        choices32.push(Box::new(|u, min_size, data_len| {
            let min = u32::try_from(min_size.saturating_mul(64 * 1024))
                .unwrap_or(u32::MAX)
                .into();
            let max = if disallow_traps { min } else { u32::MAX.into() };
            Ok(Offset::Const32(
                arbitrary_offset(u, min, max, data_len)? as i32
            ))
        }));
        let mut choices64: Vec<Box<dyn Fn(&mut Unstructured, u64, usize) -> Result<Offset>>> =
            vec![];
        choices64.push(Box::new(|u, min_size, data_len| {
            let min = min_size.saturating_mul(64 * 1024);
            let max = if disallow_traps { min } else { u64::MAX };
            Ok(Offset::Const64(
                arbitrary_offset(u, min, max, data_len)? as i64
            ))
        }));
        if !self.config.disallow_traps {
            for i in self.globals_for_const_expr(ValType::I32, true) {
                choices32.push(Box::new(move |_, _, _| Ok(Offset::Global(i))));
            }
            for i in self.globals_for_const_expr(ValType::I64, true) {
                choices64.push(Box::new(move |_, _, _| Ok(Offset::Global(i))));
            }
        }

        // Build a list of candidate memories that we'll add data initializers
        // for. If a memory doesn't have an initial size then any initializers
        // for that memory will trap instantiation, which isn't too
        // interesting. Try to make this happen less often by making it less
        // likely that a memory with 0 size will have a data segment.
        let mut memories = Vec::new();
        for (i, mem) in self.memories.iter().enumerate() {
            if mem.minimum > 0 || u.int_in_range(0..=CHANCE_SEGMENT_ON_EMPTY)? == 0 {
                memories.push(i as u32);
            }
        }

        // With memories we can generate data segments, and with bulk memory we
        // can generate passive segments. Without these though we can't create
        // a valid module with data segments.
        if memories.is_empty() && !self.config.bulk_memory_enabled {
            return Ok(());
        }

        arbitrary_loop(
            u,
            self.config.min_data_segments,
            self.config.max_data_segments,
            |u| {
                let mut init: Vec<u8> = u.arbitrary()?;

                // Passive data can only be generated if bulk memory is enabled.
                // Otherwise if there are no memories we *only* generate passive
                // data. Finally if all conditions are met we use an input byte to
                // determine if it should be passive or active.
                let kind =
                    if self.config.bulk_memory_enabled && (memories.is_empty() || u.arbitrary()?) {
                        DataSegmentKind::Passive
                    } else {
                        let memory_index = *u.choose(&memories)?;
                        let mem = &self.memories[memory_index as usize];
                        let f = if mem.memory64 {
                            u.choose(&choices64)?
                        } else {
                            u.choose(&choices32)?
                        };
                        let mut offset = f(u, mem.minimum, init.len())?;

                        // If traps are disallowed then truncate the size of the
                        // data segment to the minimum size of memory to guarantee
                        // it will fit. Afterwards ensure that the offset of the
                        // data segment is in-bounds by clamping it to the
                        if self.config.disallow_traps {
                            let max_size = (u64::MAX / 64 / 1024).min(mem.minimum) * 64 * 1024;
                            init.truncate(max_size as usize);
                            let max_offset = max_size - init.len() as u64;
                            match &mut offset {
                                Offset::Const32(x) => {
                                    *x = (*x as u64).min(max_offset) as i32;
                                }
                                Offset::Const64(x) => {
                                    *x = (*x as u64).min(max_offset) as i64;
                                }
                                Offset::Global(_) => unreachable!(),
                            }
                        }
                        DataSegmentKind::Active {
                            offset,
                            memory_index,
                        }
                    };
                self.data.push(DataSegment { kind, init });
                Ok(true)
            },
        )
    }

    fn params_results(&self, ty: &BlockType) -> (Vec<ValType>, Vec<ValType>) {
        match ty {
            BlockType::Empty => (vec![], vec![]),
            BlockType::Result(t) => (vec![], vec![*t]),
            BlockType::FunctionType(ty) => {
                let ty = self.func_type(*ty);
                (ty.params.to_vec(), ty.results.to_vec())
            }
        }
    }

    /// Returns an iterator of all globals which can be used in constant
    /// expressions for a value of type `ty` specified.
    fn globals_for_const_expr(
        &self,
        ty: ValType,
        allow_defined_globals: bool,
    ) -> impl Iterator<Item = u32> + '_ {
        // Before the GC proposal only imported globals could be referenced, but
        // the GC proposal relaxed this feature to allow any global.
        let num_imported_globals = self.globals.len() - self.defined_globals.len();
        let max_global = if self.config.gc_enabled && allow_defined_globals {
            self.globals.len()
        } else {
            num_imported_globals
        };

        self.globals[..max_global]
            .iter()
            .enumerate()
            .filter_map(move |(i, g)| {
                // Mutable globals cannot participate in constant expressions,
                // but otherwise so long as the global is a subtype of the
                // desired type it's a candidate.
                if !g.mutable && self.val_type_is_sub_type(g.val_type, ty) {
                    Some(i as u32)
                } else {
                    None
                }
            })
    }

    fn compute_interesting_values(&mut self) {
        debug_assert!(self.interesting_values32.is_empty());
        debug_assert!(self.interesting_values64.is_empty());

        let mut interesting_values32 = HashSet::new();
        let mut interesting_values64 = HashSet::new();

        let mut interesting = |val: u64| {
            interesting_values32.insert(val as u32);
            interesting_values64.insert(val);
        };

        // Zero is always interesting.
        interesting(0);

        // Max values are always interesting.
        interesting(u8::MAX as _);
        interesting(u16::MAX as _);
        interesting(u32::MAX as _);
        interesting(u64::MAX);

        // Min values are always interesting.
        interesting(i8::MIN as _);
        interesting(i16::MIN as _);
        interesting(i32::MIN as _);
        interesting(i64::MIN as _);

        for i in 0..64 {
            // Powers of two.
            interesting(1 << i);

            // Inverted powers of two.
            interesting(!(1 << i));

            // Powers of two minus one, AKA high bits unset and low bits set.
            interesting((1 << i) - 1);

            // Negative powers of two, AKA high bits set and low bits unset.
            interesting(((1_i64 << 63) >> i) as _);
        }

        // Some repeating bit patterns.
        for pattern in [0b01010101, 0b00010001, 0b00010001, 0b00000001] {
            for b in [pattern, !pattern] {
                interesting(u64::from_ne_bytes([b, b, b, b, b, b, b, b]));
            }
        }

        // Interesting float values.
        let mut interesting_f64 = |x: f64| interesting(x.to_bits());
        interesting_f64(0.0);
        interesting_f64(-0.0);
        interesting_f64(f64::INFINITY);
        interesting_f64(f64::NEG_INFINITY);
        interesting_f64(f64::EPSILON);
        interesting_f64(-f64::EPSILON);
        interesting_f64(f64::MIN);
        interesting_f64(f64::MIN_POSITIVE);
        interesting_f64(f64::MAX);
        interesting_f64(f64::NAN);
        let mut interesting_f32 = |x: f32| interesting(x.to_bits() as _);
        interesting_f32(0.0);
        interesting_f32(-0.0);
        interesting_f32(f32::INFINITY);
        interesting_f32(f32::NEG_INFINITY);
        interesting_f32(f32::EPSILON);
        interesting_f32(-f32::EPSILON);
        interesting_f32(f32::MIN);
        interesting_f32(f32::MIN_POSITIVE);
        interesting_f32(f32::MAX);
        interesting_f32(f32::NAN);

        // Interesting values related to table bounds.
        for t in self.tables.iter() {
            interesting(t.minimum as _);
            if let Some(x) = t.minimum.checked_add(1) {
                interesting(x as _);
            }

            if let Some(x) = t.maximum {
                interesting(x as _);
                if let Some(y) = x.checked_add(1) {
                    interesting(y as _);
                }
            }
        }

        // Interesting values related to memory bounds.
        for m in self.memories.iter() {
            let min = m.minimum.saturating_mul(crate::page_size(m).into());
            interesting(min);
            for i in 0..5 {
                if let Some(x) = min.checked_add(1 << i) {
                    interesting(x);
                }
                if let Some(x) = min.checked_sub(1 << i) {
                    interesting(x);
                }
            }

            if let Some(max) = m.maximum {
                let max = max.saturating_mul(crate::page_size(m).into());
                interesting(max);
                for i in 0..5 {
                    if let Some(x) = max.checked_add(1 << i) {
                        interesting(x);
                    }
                    if let Some(x) = max.checked_sub(1 << i) {
                        interesting(x);
                    }
                }
            }
        }

        self.interesting_values32.extend(interesting_values32);
        self.interesting_values64.extend(interesting_values64);

        // Sort for determinism.
        self.interesting_values32.sort();
        self.interesting_values64.sort();
    }

    fn arbitrary_const_instruction(
        &self,
        ty: ValType,
        u: &mut Unstructured<'_>,
    ) -> Result<Instruction> {
        debug_assert!(self.interesting_values32.len() > 0);
        debug_assert!(self.interesting_values64.len() > 0);
        match ty {
            ValType::I32 => Ok(Instruction::I32Const(if u.arbitrary()? {
                *u.choose(&self.interesting_values32)? as i32
            } else {
                u.arbitrary()?
            })),
            ValType::I64 => Ok(Instruction::I64Const(if u.arbitrary()? {
                *u.choose(&self.interesting_values64)? as i64
            } else {
                u.arbitrary()?
            })),
            ValType::F32 => Ok(Instruction::F32Const(if u.arbitrary()? {
                f32::from_bits(*u.choose(&self.interesting_values32)?).into()
            } else {
                u.arbitrary::<f32>()?.into()
            })),
            ValType::F64 => Ok(Instruction::F64Const(if u.arbitrary()? {
                f64::from_bits(*u.choose(&self.interesting_values64)?).into()
            } else {
                u.arbitrary::<f64>()?.into()
            })),
            ValType::V128 => Ok(Instruction::V128Const(if u.arbitrary()? {
                let upper = (*u.choose(&self.interesting_values64)? as i128) << 64;
                let lower = *u.choose(&self.interesting_values64)? as i128;
                upper | lower
            } else {
                u.arbitrary()?
            })),
            ValType::Ref(ty) => {
                assert!(ty.nullable);
                Ok(Instruction::RefNull(ty.heap_type))
            }
        }
    }

    fn propagate_shared<T>(&mut self, must_share: bool, mut f: impl FnMut(&mut Self) -> T) -> T {
        let tmp = mem::replace(&mut self.must_share, must_share);
        let result = f(self);
        self.must_share = tmp;
        result
    }

    fn arbitrary_shared(&self, u: &mut Unstructured) -> Result<bool> {
        if self.must_share {
            Ok(true)
        } else {
            Ok(self.config.shared_everything_threads_enabled && u.ratio(1, 4)?)
        }
    }

    fn is_shared_ref_type(&self, ty: RefType) -> bool {
        match ty.heap_type {
            HeapType::Abstract { shared, .. } => shared,
            HeapType::Concrete(i) => self.types[i as usize].composite_type.shared,
        }
    }

    fn is_shared_type(&self, index: u32) -> bool {
        let index = usize::try_from(index).unwrap();
        let ty = self.types.get(index).unwrap();
        ty.composite_type.shared
    }
}

pub(crate) fn arbitrary_limits64(
    u: &mut Unstructured,
    min_minimum: Option<u64>,
    max_minimum: u64,
    max_required: bool,
    max_inbounds: u64,
) -> Result<(u64, Option<u64>)> {
    assert!(
        min_minimum.unwrap_or(0) <= max_minimum,
        "{} <= {max_minimum}",
        min_minimum.unwrap_or(0),
    );
    assert!(
        min_minimum.unwrap_or(0) <= max_inbounds,
        "{} <= {max_inbounds}",
        min_minimum.unwrap_or(0),
    );

    let min = gradually_grow(u, min_minimum.unwrap_or(0), max_inbounds, max_minimum)?;
    assert!(min <= max_minimum, "{min} <= {max_minimum}");

    let max = if max_required || u.arbitrary().unwrap_or(false) {
        Some(u.int_in_range(min..=max_minimum)?)
    } else {
        None
    };
    assert!(min <= max.unwrap_or(min), "{min} <= {}", max.unwrap_or(min));

    Ok((min, max))
}

pub(crate) fn configured_valtypes(config: &Config) -> Vec<ValType> {
    let mut valtypes = Vec::with_capacity(25);
    valtypes.push(ValType::I32);
    valtypes.push(ValType::I64);
    if config.allow_floats {
        valtypes.push(ValType::F32);
        valtypes.push(ValType::F64);
    }
    if config.simd_enabled {
        valtypes.push(ValType::V128);
    }
    if config.gc_enabled && config.reference_types_enabled {
        for nullable in [
            // TODO: For now, only create allow nullable reference
            // types. Eventually we should support non-nullable reference types,
            // but this means that we will also need to recognize when it is
            // impossible to create an instance of the reference (eg `(ref
            // nofunc)` has no instances, and self-referential types that
            // contain a non-null self-reference are also impossible to create).
            true,
        ] {
            use AbstractHeapType::*;
            let abs_ref_types = [
                Any, Eq, I31, Array, Struct, None, Func, NoFunc, Extern, NoExtern,
            ];
            valtypes.extend(
                abs_ref_types
                    .iter()
                    .map(|&ty| ValType::Ref(RefType::new_abstract(ty, nullable, false))),
            );
            if config.shared_everything_threads_enabled {
                valtypes.extend(
                    abs_ref_types
                        .iter()
                        .map(|&ty| ValType::Ref(RefType::new_abstract(ty, nullable, true))),
                );
            }
        }
    } else if config.reference_types_enabled {
        valtypes.push(ValType::EXTERNREF);
        valtypes.push(ValType::FUNCREF);
    }
    valtypes
}

pub(crate) fn arbitrary_table_type(
    u: &mut Unstructured,
    config: &Config,
    module: Option<&Module>,
) -> Result<TableType> {
    let table64 = config.memory64_enabled && u.arbitrary()?;
    // We don't want to generate tables that are too large on average, so
    // keep the "inbounds" limit here a bit smaller.
    let max_inbounds = 10_000;
    let min_elements = if config.disallow_traps { Some(1) } else { None };
    let max_elements = min_elements.unwrap_or(0).max(config.max_table_elements);
    let (minimum, maximum) = arbitrary_limits64(
        u,
        min_elements,
        max_elements,
        config.table_max_size_required,
        max_inbounds.min(max_elements),
    )?;
    if config.disallow_traps {
        assert!(minimum > 0);
    }
    let element_type = match module {
        Some(module) => module.arbitrary_ref_type(u)?,
        None => RefType::FUNCREF,
    };

    // Propagate the element type's sharedness to the table type.
    let shared = match module {
        Some(module) => module.is_shared_ref_type(element_type),
        None => false,
    };

    Ok(TableType {
        element_type,
        minimum,
        maximum,
        table64,
        shared,
    })
}

pub(crate) fn arbitrary_memtype(u: &mut Unstructured, config: &Config) -> Result<MemoryType> {
    // When threads are enabled, we only want to generate shared memories about
    // 25% of the time.
    let shared = config.threads_enabled && u.ratio(1, 4)?;

    let memory64 = config.memory64_enabled && u.arbitrary()?;
    let page_size_log2 = if config.custom_page_sizes_enabled && u.arbitrary()? {
        Some(if u.arbitrary()? { 0 } else { 16 })
    } else {
        None
    };

    let min_pages = if config.disallow_traps { Some(1) } else { None };
    let max_pages = min_pages.unwrap_or(0).max(if memory64 {
        u64::try_from(config.max_memory64_bytes >> page_size_log2.unwrap_or(16))
            // Can only fail when we have a custom page size of 1 byte and a
            // memory size of `2**64 == u64::MAX + 1`. In this case, just
            // saturate to `u64::MAX`.
            .unwrap_or(u64::MAX)
    } else {
        u32::try_from(config.max_memory32_bytes >> page_size_log2.unwrap_or(16))
            // Similar case as above, but while we could represent `2**32` in our
            // `u64` here, 32-bit memories' limits must fit in a `u32`.
            .unwrap_or(u32::MAX)
            .into()
    });

    // We want to favor keeping the total memories <= 1gb in size.
    let max_all_mems_in_bytes = 1 << 30;
    let max_this_mem_in_bytes = max_all_mems_in_bytes / u64::try_from(config.max_memories).unwrap();
    let max_inbounds = max_this_mem_in_bytes >> page_size_log2.unwrap_or(16);
    let max_inbounds = max_inbounds.clamp(min_pages.unwrap_or(0), max_pages);

    let (minimum, maximum) = arbitrary_limits64(
        u,
        min_pages,
        max_pages,
        config.memory_max_size_required || shared,
        max_inbounds,
    )?;

    Ok(MemoryType {
        minimum,
        maximum,
        memory64,
        shared,
        page_size_log2,
    })
}

pub(crate) fn arbitrary_tag_type(
    u: &mut Unstructured,
    candidate_func_types: &[u32],
    get_func_type: impl FnOnce(u32) -> Rc<FuncType>,
) -> Result<TagType> {
    let max = candidate_func_types.len() - 1;
    let ty = candidate_func_types[u.int_in_range(0..=max)?];
    Ok(TagType {
        func_type_idx: ty,
        func_type: get_func_type(ty),
    })
}

/// This function generates a number between `min` and `max`, favoring values
/// between `min` and `max_inbounds`.
///
/// The thinking behind this function is that it's used for things like offsets
/// and minimum sizes which, when very large, can trivially make the wasm oom or
/// abort with a trap. This isn't the most interesting thing to do so it tries
/// to favor numbers in the `min..max_inbounds` range to avoid immediate ooms.
fn gradually_grow(u: &mut Unstructured, min: u64, max_inbounds: u64, max: u64) -> Result<u64> {
    if min == max {
        return Ok(min);
    }
    let x = {
        let min = min as f64;
        let max = max as f64;
        let max_inbounds = max_inbounds as f64;
        let x = u.arbitrary::<u32>()?;
        let x = f64::from(x);
        let x = map_custom(
            x,
            f64::from(u32::MIN)..f64::from(u32::MAX),
            min..max_inbounds,
            min..max,
        );
        assert!(min <= x, "{min} <= {x}");
        assert!(x <= max, "{x} <= {max}");
        x.round() as u64
    };

    // Conversion between `u64` and `f64` is lossy, especially for large
    // numbers, so just clamp the final result.
    return Ok(x.clamp(min, max));

    /// Map a value from within the input range to the output range(s).
    ///
    /// This will first map the input range into the `0..1` input range, and
    /// then depending on the value it will either map it exponentially
    /// (favoring small values) into the `output_inbounds` range or it will map
    /// it into the `output` range.
    fn map_custom(
        value: f64,
        input: Range<f64>,
        output_inbounds: Range<f64>,
        output: Range<f64>,
    ) -> f64 {
        assert!(!value.is_nan(), "{}", value);
        assert!(value.is_finite(), "{}", value);
        assert!(input.start < input.end, "{} < {}", input.start, input.end);
        assert!(
            output.start < output.end,
            "{} < {}",
            output.start,
            output.end
        );
        assert!(value >= input.start, "{} >= {}", value, input.start);
        assert!(value <= input.end, "{} <= {}", value, input.end);
        assert!(
            output.start <= output_inbounds.start,
            "{} <= {}",
            output.start,
            output_inbounds.start
        );
        assert!(
            output_inbounds.end <= output.end,
            "{} <= {}",
            output_inbounds.end,
            output.end
        );

        let x = map_linear(value, input, 0.0..1.0);
        let result = if x < PCT_INBOUNDS {
            if output_inbounds.start == output_inbounds.end {
                output_inbounds.start
            } else {
                let unscaled = x * x * x * x * x * x;
                map_linear(unscaled, 0.0..1.0, output_inbounds)
            }
        } else {
            map_linear(x, 0.0..1.0, output.clone())
        };

        assert!(result >= output.start, "{} >= {}", result, output.start);
        assert!(result <= output.end, "{} <= {}", result, output.end);
        result
    }

    /// Map a value from within the input range linearly to the output range.
    ///
    /// For example, mapping `0.5` from the input range `0.0..1.0` to the output
    /// range `1.0..3.0` produces `2.0`.
    fn map_linear(
        value: f64,
        Range {
            start: in_low,
            end: in_high,
        }: Range<f64>,
        Range {
            start: out_low,
            end: out_high,
        }: Range<f64>,
    ) -> f64 {
        assert!(!value.is_nan(), "{}", value);
        assert!(value.is_finite(), "{}", value);
        assert!(in_low < in_high, "{in_low} < {in_high}");
        assert!(out_low < out_high, "{out_low} < {out_high}");
        assert!(value >= in_low, "{value} >= {in_low}");
        assert!(value <= in_high, "{value} <= {in_high}");

        let dividend = out_high - out_low;
        let divisor = in_high - in_low;
        let slope = dividend / divisor;
        let result = out_low + (slope * (value - in_low));

        assert!(result >= out_low, "{result} >= {out_low}");
        assert!(result <= out_high, "{result} <= {out_high}");
        result
    }
}

/// Selects a reasonable offset for an element or data segment. This favors
/// having the segment being in-bounds, but it may still generate
/// any offset.
fn arbitrary_offset(
    u: &mut Unstructured,
    limit_min: u64,
    limit_max: u64,
    segment_size: usize,
) -> Result<u64> {
    let size = u64::try_from(segment_size).unwrap();

    // If the segment is too big for the whole memory, just give it any
    // offset.
    if size > limit_min {
        u.int_in_range(0..=limit_max)
    } else {
        gradually_grow(u, 0, limit_min - size, limit_max)
    }
}

fn unique_import_strings(max_size: usize, u: &mut Unstructured) -> Result<(String, String)> {
    let module = limited_string(max_size, u)?;
    let field = limited_string(max_size, u)?;
    Ok((module, field))
}

fn arbitrary_vec_u8(u: &mut Unstructured) -> Result<Vec<u8>> {
    let size = u.arbitrary_len::<u8>()?;
    Ok(u.bytes(size)?.to_vec())
}

impl EntityType {
    fn size(&self) -> u32 {
        match self {
            EntityType::Tag(_)
            | EntityType::Global(_)
            | EntityType::Table(_)
            | EntityType::Memory(_) => 1,
            EntityType::Func(_, ty) => 1 + (ty.params.len() + ty.results.len()) as u32,
        }
    }
}

/// A container for the kinds of instructions that wasm-smith is allowed to
/// emit.
///
/// # Example
///
/// ```
/// # use wasm_smith::{InstructionKinds, InstructionKind};
/// let kinds = InstructionKinds::new(&[InstructionKind::Numeric, InstructionKind::Memory]);
/// assert!(kinds.contains(InstructionKind::Memory));
/// ```
#[derive(Clone, Copy, Debug, Default)]
#[cfg_attr(
    feature = "serde",
    derive(serde_derive::Deserialize, serde_derive::Serialize)
)]
pub struct InstructionKinds(pub(crate) FlagSet<InstructionKind>);

impl InstructionKinds {
    /// Create a new container.
    pub fn new(kinds: &[InstructionKind]) -> Self {
        Self(kinds.iter().fold(FlagSet::default(), |ks, k| ks | *k))
    }

    /// Include all [InstructionKind]s.
    pub fn all() -> Self {
        Self(FlagSet::full())
    }

    /// Include no [InstructionKind]s.
    pub fn none() -> Self {
        Self(FlagSet::default())
    }

    /// Check if the [InstructionKind] is contained in this set.
    #[inline]
    pub fn contains(&self, kind: InstructionKind) -> bool {
        self.0.contains(kind)
    }

    /// Restrict each [InstructionKind] to its subset not involving floats
    pub fn without_floats(&self) -> Self {
        let mut floatless = self.0;
        if floatless.contains(InstructionKind::Numeric) {
            floatless -= InstructionKind::Numeric;
            floatless |= InstructionKind::NumericInt;
        }
        if floatless.contains(InstructionKind::Vector) {
            floatless -= InstructionKind::Vector;
            floatless |= InstructionKind::VectorInt;
        }
        if floatless.contains(InstructionKind::Memory) {
            floatless -= InstructionKind::Memory;
            floatless |= InstructionKind::MemoryInt;
        }
        Self(floatless)
    }
}

flags! {
    /// Enumerate the categories of instructions defined in the [WebAssembly
    /// specification](https://webassembly.github.io/spec/core/syntax/instructions.html).
    #[allow(missing_docs)]
    #[cfg_attr(feature = "_internal_cli", derive(serde_derive::Deserialize))]
    pub enum InstructionKind: u16 {
        NumericInt = 1 << 0,
        Numeric = (1 << 1) | (1 << 0),
        VectorInt = 1 << 2,
        Vector = (1 << 3) | (1 << 2),
        Reference = 1 << 4,
        Parametric = 1 << 5,
        Variable = 1 << 6,
        Table = 1 << 7,
        MemoryInt = 1 << 8,
        Memory = (1 << 9) | (1 << 8),
        Control = 1 << 10,
        Aggregate = 1 << 11,
    }
}

impl FromStr for InstructionKinds {
    type Err = String;
    fn from_str(s: &str) -> std::prelude::v1::Result<Self, Self::Err> {
        let mut kinds = vec![];
        for part in s.split(",") {
            let kind = InstructionKind::from_str(part)?;
            kinds.push(kind);
        }
        Ok(InstructionKinds::new(&kinds))
    }
}

impl FromStr for InstructionKind {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "numeric_non_float" => Ok(InstructionKind::NumericInt),
            "numeric" => Ok(InstructionKind::Numeric),
            "vector_non_float" => Ok(InstructionKind::VectorInt),
            "vector" => Ok(InstructionKind::Vector),
            "reference" => Ok(InstructionKind::Reference),
            "parametric" => Ok(InstructionKind::Parametric),
            "variable" => Ok(InstructionKind::Variable),
            "table" => Ok(InstructionKind::Table),
            "memory_non_float" => Ok(InstructionKind::MemoryInt),
            "memory" => Ok(InstructionKind::Memory),
            "control" => Ok(InstructionKind::Control),
            _ => Err(format!("unknown instruction kind: {s}")),
        }
    }
}

// Conversions from `wasmparser` to `wasm-smith`. Currently, only type conversions
// have been implemented.
#[cfg(feature = "wasmparser")]
impl TryFrom<wasmparser::FuncType> for FuncType {
    type Error = ();

    fn try_from(value: wasmparser::FuncType) -> Result<Self, Self::Error> {
        Ok(FuncType {
            params: value
                .params()
                .iter()
                .copied()
                .map(|ty| ty.try_into().map_err(|_| ()))
                .collect::<Result<Vec<_>, _>>()?,
            results: value
                .results()
                .iter()
                .copied()
                .map(|ty| ty.try_into().map_err(|_| ()))
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

#[cfg(feature = "wasmparser")]
impl TryFrom<wasmparser::CompositeType> for CompositeType {
    type Error = ();

    fn try_from(value: wasmparser::CompositeType) -> Result<Self, Self::Error> {
        let inner_type = match value.inner {
            wasmparser::CompositeInnerType::Func(func_type) => {
                CompositeInnerType::Func(Rc::new(func_type.try_into()?))
            }
            wasmparser::CompositeInnerType::Array(array_type) => {
                CompositeInnerType::Array(array_type.try_into().map_err(|_| ())?)
            }
            wasmparser::CompositeInnerType::Struct(struct_type) => {
                CompositeInnerType::Struct(struct_type.try_into().map_err(|_| ())?)
            }
            wasmparser::CompositeInnerType::Cont(_) => {
                panic!("continuation type is not supported by wasm-smith currently.")
            }
        };

        Ok(CompositeType {
            inner: inner_type,
            shared: value.shared,
        })
    }
}

#[cfg(feature = "wasmparser")]
impl TryFrom<wasmparser::SubType> for SubType {
    type Error = ();

    fn try_from(value: wasmparser::SubType) -> Result<Self, Self::Error> {
        Ok(SubType {
            is_final: value.is_final,
            supertype: value
                .supertype_idx
                .map(|idx| idx.as_module_index().ok_or(()))
                .transpose()?,
            composite_type: value.composite_type.try_into()?,
            // We cannot determine the depth of current subtype here, set it to 1
            // temporarily and fix it later.
            depth: 1,
        })
    }
}
