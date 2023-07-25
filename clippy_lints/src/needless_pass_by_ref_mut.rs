use super::needless_pass_by_value::requires_exact_signature;
use clippy_utils::diagnostics::span_lint_hir_and_then;
use clippy_utils::source::snippet;
use clippy_utils::{get_parent_node, is_from_proc_macro, is_self};
use rustc_data_structures::fx::{FxHashSet, FxIndexMap};
use rustc_errors::Applicability;
use rustc_hir::intravisit::{walk_qpath, FnKind, Visitor};
use rustc_hir::{Body, ExprKind, FnDecl, HirId, HirIdMap, HirIdSet, Impl, ItemKind, Mutability, Node, PatKind, QPath};
use rustc_hir_typeck::expr_use_visitor as euv;
use rustc_infer::infer::TyCtxtInferExt;
use rustc_lint::{LateContext, LateLintPass};
use rustc_middle::hir::map::associated_body;
use rustc_middle::hir::nested_filter::OnlyBodies;
use rustc_middle::mir::FakeReadCause;
use rustc_middle::ty::{self, Ty, TyCtxt, UpvarId, UpvarPath};
use rustc_session::{declare_tool_lint, impl_lint_pass};
use rustc_span::def_id::{LocalDefId, CRATE_DEF_ID};
use rustc_span::symbol::kw;
use rustc_span::{sym, Span};
use rustc_target::spec::abi::Abi;

declare_clippy_lint! {
    /// ### What it does
    /// Check if a `&mut` function argument is actually used mutably.
    ///
    /// Be careful if the function is publicly reexported as it would break compatibility with
    /// users of this function.
    ///
    /// ### Why is this bad?
    /// Less `mut` means less fights with the borrow checker. It can also lead to more
    /// opportunities for parallelization.
    ///
    /// ### Example
    /// ```rust
    /// fn foo(y: &mut i32) -> i32 {
    ///     12 + *y
    /// }
    /// ```
    /// Use instead:
    /// ```rust
    /// fn foo(y: &i32) -> i32 {
    ///     12 + *y
    /// }
    /// ```
    #[clippy::version = "1.72.0"]
    pub NEEDLESS_PASS_BY_REF_MUT,
    suspicious,
    "using a `&mut` argument when it's not mutated"
}

#[derive(Clone)]
pub struct NeedlessPassByRefMut<'tcx> {
    avoid_breaking_exported_api: bool,
    used_fn_def_ids: FxHashSet<LocalDefId>,
    fn_def_ids_to_maybe_unused_mut: FxIndexMap<LocalDefId, Vec<rustc_hir::Ty<'tcx>>>,
}

impl NeedlessPassByRefMut<'_> {
    pub fn new(avoid_breaking_exported_api: bool) -> Self {
        Self {
            avoid_breaking_exported_api,
            used_fn_def_ids: FxHashSet::default(),
            fn_def_ids_to_maybe_unused_mut: FxIndexMap::default(),
        }
    }
}

impl_lint_pass!(NeedlessPassByRefMut<'_> => [NEEDLESS_PASS_BY_REF_MUT]);

fn should_skip<'tcx>(
    cx: &LateContext<'tcx>,
    input: rustc_hir::Ty<'tcx>,
    ty: Ty<'_>,
    arg: &rustc_hir::Param<'_>,
) -> bool {
    // We check if this a `&mut`. `ref_mutability` returns `None` if it's not a reference.
    if !matches!(ty.ref_mutability(), Some(Mutability::Mut)) {
        return true;
    }

    if is_self(arg) {
        return true;
    }

    if let PatKind::Binding(.., name, _) = arg.pat.kind {
        // If it's a potentially unused variable, we don't check it.
        if name.name == kw::Underscore || name.as_str().starts_with('_') {
            return true;
        }
    }

    // All spans generated from a proc-macro invocation are the same...
    is_from_proc_macro(cx, &input)
}

fn inherits_cfg(tcx: TyCtxt<'_>, def_id: LocalDefId) -> bool {
    if def_id == CRATE_DEF_ID {
        false
    } else if tcx.has_attr(def_id, sym::cfg) {
        true
    } else {
        inherits_cfg(tcx, tcx.parent_module_from_def_id(def_id))
    }
}

impl<'tcx> LateLintPass<'tcx> for NeedlessPassByRefMut<'tcx> {
    fn check_fn(
        &mut self,
        cx: &LateContext<'tcx>,
        kind: FnKind<'tcx>,
        decl: &'tcx FnDecl<'tcx>,
        body: &'tcx Body<'_>,
        span: Span,
        fn_def_id: LocalDefId,
    ) {
        if span.from_expansion() {
            return;
        }

        let hir_id = cx.tcx.hir().local_def_id_to_hir_id(fn_def_id);
        let is_async = match kind {
            FnKind::ItemFn(.., header) => {
                let attrs = cx.tcx.hir().attrs(hir_id);
                if header.abi != Abi::Rust || requires_exact_signature(attrs) {
                    return;
                }
                header.is_async()
            },
            FnKind::Method(.., sig) => sig.header.is_async(),
            FnKind::Closure => return,
        };

        // Exclude non-inherent impls
        if let Some(Node::Item(item)) = cx.tcx.hir().find_parent(hir_id) {
            if matches!(
                item.kind,
                ItemKind::Impl(Impl { of_trait: Some(_), .. }) | ItemKind::Trait(..)
            ) {
                return;
            }
        }

        let fn_sig = cx.tcx.fn_sig(fn_def_id).subst_identity();
        let fn_sig = cx.tcx.liberate_late_bound_regions(fn_def_id.to_def_id(), fn_sig);

        // If there are no `&mut` argument, no need to go any further.
        let mut it = decl
            .inputs
            .iter()
            .zip(fn_sig.inputs())
            .zip(body.params)
            .filter(|((&input, &ty), arg)| !should_skip(cx, input, ty, arg))
            .peekable();
        if it.peek().is_none() {
            return;
        }
        // Collect variables mutably used and spans which will need dereferencings from the
        // function body.
        let MutablyUsedVariablesCtxt { mutably_used_vars, .. } = {
            let mut ctx = MutablyUsedVariablesCtxt::default();
            let infcx = cx.tcx.infer_ctxt().build();
            euv::ExprUseVisitor::new(&mut ctx, &infcx, fn_def_id, cx.param_env, cx.typeck_results()).consume_body(body);
            if is_async {
                let closures = ctx.async_closures.clone();
                let hir = cx.tcx.hir();
                for closure in closures {
                    ctx.prev_bind = None;
                    ctx.prev_move_to_closure.clear();
                    if let Some(body) = hir
                        .find_by_def_id(closure)
                        .and_then(associated_body)
                        .map(|(_, body_id)| hir.body(body_id))
                    {
                        euv::ExprUseVisitor::new(&mut ctx, &infcx, closure, cx.param_env, cx.typeck_results())
                            .consume_body(body);
                    }
                }
            }
            ctx
        };
        for ((&input, &_), arg) in it {
            // Only take `&mut` arguments.
            if let PatKind::Binding(_, canonical_id, ..) = arg.pat.kind
                && !mutably_used_vars.contains(&canonical_id)
            {
                self.fn_def_ids_to_maybe_unused_mut.entry(fn_def_id).or_default().push(input);
            }
        }
    }

    fn check_crate_post(&mut self, cx: &LateContext<'tcx>) {
        cx.tcx.hir().visit_all_item_likes_in_crate(&mut FnNeedsMutVisitor {
            cx,
            used_fn_def_ids: &mut self.used_fn_def_ids,
        });

        for (fn_def_id, unused) in self
            .fn_def_ids_to_maybe_unused_mut
            .iter()
            .filter(|(def_id, _)| !self.used_fn_def_ids.contains(def_id))
        {
            let show_semver_warning =
                self.avoid_breaking_exported_api && cx.effective_visibilities.is_exported(*fn_def_id);

            let mut is_cfged = None;
            for input in unused {
                // If the argument is never used mutably, we emit the warning.
                let sp = input.span;
                if let rustc_hir::TyKind::Ref(_, inner_ty) = input.kind {
                    let is_cfged = is_cfged.get_or_insert_with(|| inherits_cfg(cx.tcx, *fn_def_id));
                    span_lint_hir_and_then(
                        cx,
                        NEEDLESS_PASS_BY_REF_MUT,
                        cx.tcx.hir().local_def_id_to_hir_id(*fn_def_id),
                        sp,
                        "this argument is a mutable reference, but not used mutably",
                        |diag| {
                            diag.span_suggestion(
                                sp,
                                "consider changing to".to_string(),
                                format!("&{}", snippet(cx, cx.tcx.hir().span(inner_ty.ty.hir_id), "_"),),
                                Applicability::Unspecified,
                            );
                            if show_semver_warning {
                                diag.warn("changing this function will impact semver compatibility");
                            }
                            if *is_cfged {
                                diag.note("this is cfg-gated and may require further changes");
                            }
                        },
                    );
                }
            }
        }
    }
}

#[derive(Default)]
struct MutablyUsedVariablesCtxt {
    mutably_used_vars: HirIdSet,
    prev_bind: Option<HirId>,
    prev_move_to_closure: HirIdSet,
    aliases: HirIdMap<HirId>,
    async_closures: FxHashSet<LocalDefId>,
}

impl MutablyUsedVariablesCtxt {
    fn add_mutably_used_var(&mut self, mut used_id: HirId) {
        while let Some(id) = self.aliases.get(&used_id) {
            self.mutably_used_vars.insert(used_id);
            used_id = *id;
        }
        self.mutably_used_vars.insert(used_id);
    }
}

impl<'tcx> euv::Delegate<'tcx> for MutablyUsedVariablesCtxt {
    fn consume(&mut self, cmt: &euv::PlaceWithHirId<'tcx>, _id: HirId) {
        if let euv::Place {
            base:
                euv::PlaceBase::Local(vid)
                | euv::PlaceBase::Upvar(UpvarId {
                    var_path: UpvarPath { hir_id: vid },
                    ..
                }),
            base_ty,
            ..
        } = &cmt.place
        {
            if let Some(bind_id) = self.prev_bind.take() {
                if bind_id != *vid {
                    self.aliases.insert(bind_id, *vid);
                }
            } else if !self.prev_move_to_closure.contains(vid)
                && matches!(base_ty.ref_mutability(), Some(Mutability::Mut))
            {
                self.add_mutably_used_var(*vid);
            }
            self.prev_bind = None;
            self.prev_move_to_closure.remove(vid);
        }
    }

    fn borrow(&mut self, cmt: &euv::PlaceWithHirId<'tcx>, _id: HirId, borrow: ty::BorrowKind) {
        self.prev_bind = None;
        if let euv::Place {
            base: euv::PlaceBase::Local(vid),
            base_ty,
            ..
        } = &cmt.place
        {
            // If this is a mutable borrow, it was obviously used mutably so we add it. However
            // for `UniqueImmBorrow`, it's interesting because if you do: `array[0] = value` inside
            // a closure, it'll return this variant whereas if you have just an index access, it'll
            // return `ImmBorrow`. So if there is "Unique" and it's a mutable reference, we add it
            // to the mutably used variables set.
            if borrow == ty::BorrowKind::MutBorrow
                || (borrow == ty::BorrowKind::UniqueImmBorrow && base_ty.ref_mutability() == Some(Mutability::Mut))
            {
                self.add_mutably_used_var(*vid);
            }
        }
    }

    fn mutate(&mut self, cmt: &euv::PlaceWithHirId<'tcx>, _id: HirId) {
        self.prev_bind = None;
        if let euv::Place {
            projections,
            base: euv::PlaceBase::Local(vid),
            ..
        } = &cmt.place
        {
            if !projections.is_empty() {
                self.add_mutably_used_var(*vid);
            }
        }
    }

    fn copy(&mut self, _cmt: &euv::PlaceWithHirId<'tcx>, _id: HirId) {
        self.prev_bind = None;
    }

    fn fake_read(
        &mut self,
        cmt: &rustc_hir_typeck::expr_use_visitor::PlaceWithHirId<'tcx>,
        cause: FakeReadCause,
        _id: HirId,
    ) {
        if let euv::Place {
            base:
                euv::PlaceBase::Upvar(UpvarId {
                    var_path: UpvarPath { hir_id: vid },
                    ..
                }),
            ..
        } = &cmt.place
        {
            if let FakeReadCause::ForLet(Some(inner)) = cause {
                // Seems like we are inside an async function. We need to store the closure `DefId`
                // to go through it afterwards.
                self.async_closures.insert(inner);
                self.aliases.insert(cmt.hir_id, *vid);
                self.prev_move_to_closure.insert(*vid);
            }
        }
    }

    fn bind(&mut self, _cmt: &euv::PlaceWithHirId<'tcx>, id: HirId) {
        self.prev_bind = Some(id);
    }
}

/// A final pass to check for paths referencing this function that require the argument to be
/// `&mut`, basically if the function is ever used as a `fn`-like argument.
struct FnNeedsMutVisitor<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,
    used_fn_def_ids: &'a mut FxHashSet<LocalDefId>,
}

impl<'tcx> Visitor<'tcx> for FnNeedsMutVisitor<'_, 'tcx> {
    type NestedFilter = OnlyBodies;

    fn nested_visit_map(&mut self) -> Self::Map {
        self.cx.tcx.hir()
    }

    fn visit_qpath(&mut self, qpath: &'tcx QPath<'tcx>, hir_id: HirId, _: Span) {
        walk_qpath(self, qpath, hir_id);

        let Self { cx, used_fn_def_ids } = self;

        // #11182; do not lint if mutability is required elsewhere
        if let Node::Expr(expr) = cx.tcx.hir().get(hir_id)
            && let Some(parent) = get_parent_node(cx.tcx, expr.hir_id)
            && let ty::FnDef(def_id, _) = cx.tcx.typeck(cx.tcx.hir().enclosing_body_owner(hir_id)).expr_ty(expr).kind()
            && let Some(def_id) = def_id.as_local()
        {
            if let Node::Expr(e) = parent
                && let ExprKind::Call(call, _) = e.kind
                && call.hir_id == expr.hir_id
            {
                return;
            }

            // We don't need to check each argument individually as you cannot coerce a function
            // taking `&mut` -> `&`, for some reason, so if we've gotten this far we know it's
            // passed as a `fn`-like argument (or is unified) and should ignore every "unused"
            // argument entirely
            used_fn_def_ids.insert(def_id);
        }
    }
}
