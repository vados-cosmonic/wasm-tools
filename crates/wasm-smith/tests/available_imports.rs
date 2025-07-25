#![cfg(feature = "wasmparser")]

use arbitrary::Unstructured;
use rand::{RngCore, SeedableRng, rngs::SmallRng};
use std::collections::HashMap;
use wasm_smith::{Config, Module};
use wasmparser::Validator;
use wasmparser::{Parser, TypeRef, ValType};

mod common;
use common::validate;

#[test]
fn smoke_test_imports_config() {
    let mut n_partial = 0;
    let mut global_imports_seen = HashMap::<_, bool>::new();
    let mut rng = SmallRng::seed_from_u64(11);
    let mut buf = vec![0; 512];
    for _ in 0..1024 {
        rng.fill_bytes(&mut buf);

        let mut u = Unstructured::new(&buf);
        let (config, available) = import_config(&mut u);
        let features = config.features();

        if let Ok(module) = Module::new(config, &mut u) {
            let wasm_bytes = module.to_bytes();
            let mut validator = Validator::new_with_features(features);
            validate(&mut validator, &wasm_bytes);
            let mut imports_seen = available
                .iter()
                .map(|(m, f, t)| ((*m, *f), (false, t)))
                .collect::<HashMap<_, _>>();
            let mut sig_types = Vec::new();

            for payload in Parser::new(0).parse_all(&wasm_bytes) {
                let payload = payload.unwrap();
                if let wasmparser::Payload::TypeSection(rdr) = payload {
                    // Gather the signature types to later check function types
                    // against.
                    for recgrp in rdr {
                        let recgrp = recgrp.unwrap();
                        for subtype in recgrp.into_types() {
                            match subtype.composite_type.inner {
                                wasmparser::CompositeInnerType::Func(func_type) => {
                                    sig_types.push(Some(func_type))
                                }
                                _ => sig_types.push(None),
                            }
                        }
                    }
                } else if let wasmparser::Payload::ImportSection(rdr) = payload {
                    // Read out imports, checking that they all are within the
                    // list of expected imports (i.e. we don't generate
                    // arbitrary ones), and that we handle the logic correctly
                    // (i.e. signature types are as expected)
                    for import in rdr {
                        let import = import.unwrap();
                        use AvailableImportKind as I;
                        let entry = imports_seen.get_mut(&(import.module, import.name));
                        match (entry, &import.ty) {
                            (Some((true, _)), _) => panic!("duplicate import of {import:?}"),
                            (Some((seen, I::Memory)), TypeRef::Memory(_)) => *seen = true,
                            (Some((seen, I::Global(t))), TypeRef::Global(gt))
                                if *t == gt.content_type =>
                            {
                                *seen = true
                            }
                            (Some((seen, I::Table(t))), TypeRef::Table(tt))
                                if *t == ValType::Ref(tt.element_type) =>
                            {
                                *seen = true
                            }
                            (Some((seen, I::Func(p, r))), TypeRef::Func(sig_idx))
                                if sig_types[*sig_idx as usize].clone().unwrap().params() == *p
                                    && sig_types[*sig_idx as usize].clone().unwrap().results()
                                        == *r =>
                            {
                                *seen = true
                            }
                            (
                                Some((seen, I::Tag(p))),
                                TypeRef::Tag(wasmparser::TagType { func_type_idx, .. }),
                            ) if sig_types[*func_type_idx as usize].clone().unwrap().params()
                                == *p
                                && sig_types[*func_type_idx as usize]
                                    .clone()
                                    .unwrap()
                                    .results()
                                    .is_empty() =>
                            {
                                *seen = true
                            }
                            (Some((_, expected)), _) => {
                                panic!("import {import:?} type mismatch, expected: {expected:?}")
                            }
                            (None, _) => panic!("import of an unknown entity: {import:?}"),
                        }
                    }
                }
            }

            // Verify that we have seen both instances with partial imports
            // (i.e. we don't always just copy over all the imports from the
            // example module) and also that we eventually observe all of the
            // imports being used (i.e. selection is reasonably random)
            for (m, f, _) in &available[..] {
                let seen = imports_seen[&(*m, *f)];
                let global_seen = global_imports_seen
                    .entry((m.to_string(), f.to_string()))
                    .or_default();
                *global_seen |= seen.0;
            }
            if !imports_seen.values().all(|v| v.0) {
                n_partial += 1;
            }
        }
    }
    assert!(global_imports_seen.values().all(|v| *v));
    assert!(n_partial > 0);
}

#[derive(Debug)]
enum AvailableImportKind {
    Func(Vec<ValType>, Vec<ValType>),
    Tag(Vec<ValType>),
    Global(ValType),
    Table(ValType),
    Memory,
}

fn import_config(
    u: &mut Unstructured,
) -> (
    Config,
    Vec<(&'static str, &'static str, AvailableImportKind)>,
) {
    let mut config = Config::default();
    config.exceptions_enabled = u.arbitrary().expect("exceptions enabled for swarm");
    let available = {
        use {
            AvailableImportKind::*,
            ValType::*,
            wasmparser::{PackedIndex, RefType},
        };
        vec![
            ("env", "pi", Func(vec![I32], vec![])),
            ("env", "pi2", Func(vec![I32], vec![])),
            ("env", "pipi2", Func(vec![I32, I32], vec![])),
            ("env", "po", Func(vec![], vec![I32])),
            ("env", "pipo", Func(vec![I32], vec![I32])),
            ("env", "popo", Func(vec![], vec![I32, I32])),
            ("env", "mem", Memory),
            ("env", "tbl", Table(ValType::FUNCREF)),
            ("vars", "g", Global(I64)),
            ("tags", "tag1", Tag(vec![I32])),
            (
                "tags",
                "tag2",
                Tag(vec![Ref(RefType::concrete(
                    true,
                    PackedIndex::from_module_index(0).unwrap(),
                ))]),
            ),
        ]
    };
    config.available_imports = Some(
        wat::parse_str(
            r#"
            (module
                (rec
                    (type $s0 (array (ref $s1)))
                    (type $s1 (array (ref $s0)))
                )
                (import "env" "pi" (func (param i32)))
                (import "env" "pi2" (func (param i32)))
                (import "env" "pipi2" (func (param i32 i32)))
                (import "env" "po" (func (result i32)))
                (import "env" "pipo" (func (param i32) (result i32)))
                (import "env" "popo" (func (result i32 i32)))
                (import "env" "mem" (memory 1 16))
                (import "env" "tbl" (table 1 16 funcref))
                (import "vars" "g" (global i64))
                (import "tags" "tag1" (tag (param i32)))
                (import "tags" "tag2" (tag (param (ref null $s0))))
            )
            "#,
        )
        .unwrap()
        .into(),
    );
    (config, available)
}
