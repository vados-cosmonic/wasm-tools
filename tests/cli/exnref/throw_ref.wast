;; RUN: wast --assert default --snapshot tests/snapshots %

(module
  (func (param exnref)
    local.get 0
    throw_ref
  )
)
