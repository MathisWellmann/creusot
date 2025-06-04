use super::pearlite::{Term, TermKind};
use crate::{
    contracts_items::{is_law, is_pearlite, is_spec},
    ctx::*,
    naming::name,
    util::erased_identity_for_item,
    very_stable_hash::get_very_stable_hash,
};
use rustc_hir::def_id::DefId;
use rustc_infer::{
    infer::{DefineOpaqueTypes, InferCtxt, TyCtxtInferExt},
    traits::{Obligation, ObligationCause, TraitEngine},
};
use rustc_middle::ty::{
    Const, ConstKind, EarlyBinder, GenericArgsRef, ParamConst, ParamEnv, ParamTy, Predicate,
    TraitRef, Ty, TyCtxt, TyKind, TypeFoldable, TypeFolder, TypingEnv, TypingMode,
};
use rustc_span::{DUMMY_SP, Span};
use rustc_trait_selection::{
    error_reporting::InferCtxtErrorExt,
    traits::{FulfillmentError, ImplSource, InCrate, TraitEngineExt, orphan_check_trait_ref},
};
use rustc_type_ir::fold::TypeSuperFoldable;
use std::collections::HashMap;

#[derive(Clone)]
pub(crate) struct Refinement<'tcx> {
    #[allow(dead_code)]
    pub(crate) trait_: (DefId, GenericArgsRef<'tcx>),
    pub(crate) impl_: (DefId, GenericArgsRef<'tcx>),
    pub(crate) refn: Term<'tcx>,
}

#[allow(dead_code)]
#[derive(Clone)]
pub(crate) struct TraitImpl<'tcx> {
    pub(crate) laws: Vec<DefId>,
    pub(crate) refinements: Vec<Refinement<'tcx>>,
}

impl<'tcx> TranslationCtx<'tcx> {
    pub(crate) fn laws_inner(&self, trait_or_impl: DefId) -> Vec<DefId> {
        let mut laws = Vec::new();
        for item in self
            .tcx
            .associated_items(trait_or_impl)
            .in_definition_order()
            .filter(move |item| !is_spec(self.tcx, item.def_id))
        {
            if is_law(self.tcx, item.def_id) {
                laws.push(item.def_id);
            }
        }
        laws
    }

    pub(crate) fn translate_impl(&self, impl_id: DefId) -> TraitImpl<'tcx> {
        assert!(self.trait_id_of_impl(impl_id).is_some(), "{impl_id:?} is not a trait impl");
        let trait_ref = self.tcx.impl_trait_ref(impl_id).unwrap().instantiate_identity();

        let mut laws = Vec::new();
        let implementor_map = self.tcx.impl_item_implementor_ids(impl_id);

        let mut refinements = Vec::new();
        let mut implementor_map =
            self.with_stable_hashing_context(|hcx| implementor_map.to_sorted(&hcx, true));
        implementor_map.sort_by_cached_key(|(trait_item, impl_item)| {
            get_very_stable_hash(&[**trait_item, **impl_item] as &[_], &self.tcx)
        });
        for (&trait_item, &impl_item) in implementor_map {
            if is_law(self.tcx, trait_item) {
                laws.push(impl_item);
            }

            // Don't generate refinements for impls that come from outside crates
            if !impl_id.is_local() {
                continue;
            }

            let subst = erased_identity_for_item(self.tcx, impl_item);

            let refn_subst = subst.rebase_onto(self.tcx, impl_id, trait_ref.args);

            if !self.tcx.def_kind(trait_item).is_fn_like() {
                continue;
            }

            // TODO: Clean up and abstract
            let predicates = self
                .extern_spec(trait_item)
                .map(|p| p.predicates_for(self.tcx, refn_subst))
                .unwrap_or_else(Vec::new);

            let infcx =
                self.tcx.infer_ctxt().ignoring_regions().build(TypingMode::non_body_analysis());

            let res = evaluate_additional_predicates(
                &infcx,
                predicates,
                self.param_env(impl_item),
                self.def_span(impl_item),
            );
            if let Err(errs) = res {
                infcx.err_ctxt().report_fulfillment_errors(errs);
                self.crash_and_error(rustc_span::DUMMY_SP, "error above");
            }

            let refn = logic_refinement_term(self, impl_item, trait_item, refn_subst);
            refinements.push(Refinement {
                trait_: (trait_item, refn_subst),
                impl_: (impl_item, subst),
                refn,
            });
        }

        TraitImpl { laws, refinements }
    }
}

fn logic_refinement_term<'tcx>(
    ctx: &TranslationCtx<'tcx>,
    impl_item_id: DefId,
    trait_item_id: DefId,
    refn_subst: GenericArgsRef<'tcx>,
) -> Term<'tcx> {
    let typing_env = TypingEnv::non_body_analysis(ctx.tcx, impl_item_id);

    // Get the contract of the trait version
    let mut trait_sig = EarlyBinder::bind(ctx.sig(trait_item_id).clone())
        .instantiate(ctx.tcx, refn_subst)
        .normalize(ctx.tcx, typing_env);

    let mut impl_sig = ctx.sig(impl_item_id).clone();

    if !is_pearlite(ctx.tcx, impl_item_id) {
        trait_sig.add_type_invariant_spec(ctx, trait_item_id, typing_env);
        impl_sig.add_type_invariant_spec(ctx, impl_item_id, typing_env);
    }

    let span = ctx.tcx.def_span(impl_item_id);
    let mut args = Vec::new();
    let mut subst = HashMap::new();
    for (&(id, _, _), (id2, _, ty)) in trait_sig.inputs.iter().zip(impl_sig.inputs.iter()) {
        args.push((id, *ty));
        subst.insert(id2.0, TermKind::Var(id));
    }

    let mut impl_precond = impl_sig.contract.requires_conj(ctx.tcx);
    impl_precond.subst(&subst);
    let trait_precond = trait_sig.contract.requires_conj(ctx.tcx);

    let mut impl_postcond = impl_sig.contract.ensures_conj(ctx.tcx);
    impl_postcond.subst(&subst);
    let trait_postcond = trait_sig.contract.ensures_conj(ctx.tcx);

    let retty = impl_sig.output;

    let post_refn =
        impl_postcond.implies(trait_postcond).forall((name::result().into(), retty)).span(span);

    let mut refn = trait_precond.implies(impl_precond.conj(post_refn));
    refn = args.into_iter().rfold(refn, |acc, r| acc.forall(r).span(span));

    refn
}

pub(crate) fn evaluate_additional_predicates<'tcx>(
    infcx: &InferCtxt<'tcx>,
    p: Vec<Predicate<'tcx>>,
    param_env: ParamEnv<'tcx>,
    sp: Span,
) -> Result<(), Vec<FulfillmentError<'tcx>>> {
    let mut fulfill_cx = <dyn TraitEngine<'tcx, _>>::new(infcx);
    for predicate in p {
        let predicate = infcx.tcx.erase_regions(predicate);
        let cause = ObligationCause::dummy_with_span(sp);
        let obligation = Obligation { cause, param_env, recursion_depth: 0, predicate };
        fulfill_cx.register_predicate_obligation(infcx, obligation);
    }
    let errors = fulfill_cx.select_all_or_error(infcx);
    if !errors.is_empty() { Err(errors) } else { Ok(()) }
}

/// The result of [`Self::resolve_assoc_item_opt`]: given the id of a trait item and some
/// type parameters, we might find an actual implementation of the item.
#[derive(Debug)]
pub(crate) enum TraitResolved<'tcx> {
    NotATraitItem,
    /// An instance (like `impl Clone for i32 { ... }`) exists for the given type parameters.
    Instance(DefId, GenericArgsRef<'tcx>),
    /// A known instance exists, but we don't know which one.
    UnknownFound,
    /// We don't know if an instance exists.
    UnknownNotFound,
    /// We know that no instance exists.
    ///
    /// For example, in `fn<T> f(x: T) { let _ = x.clone() }`, we  don't have an
    /// instance for `T::clone` until we know more about `T`.
    NoInstance,
}

impl<'tcx> TraitResolved<'tcx> {
    /// Try to resolve a trait item to the item in an `impl` block, given some typing context.
    ///
    /// # Parameters
    /// - `tcx`: The global context
    /// - `typing_env`: The scope of type variables, see <https://rustc-dev-guide.rust-lang.org/param_env/param_env_summary.html>.
    /// - `trait_item_def_id`: The trait item we are trying to resolve.
    /// - `substs`: The type parameters we are instantiating the trait item with. This
    ///   can include the `Self` parameter.
    pub(crate) fn resolve_item(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        trait_item_def_id: DefId,
        substs: GenericArgsRef<'tcx>,
    ) -> Self {
        trace!("TraitResolved::resolve {:?} {:?}", trait_item_def_id, substs);

        let trait_ref = if let Some(did) = tcx.trait_of_item(trait_item_def_id) {
            TraitRef::from_method(tcx, did, substs)
        } else {
            return TraitResolved::NotATraitItem;
        };
        let trait_ref = tcx.normalize_erasing_regions(typing_env, trait_ref);

        let source = if let Ok(source) =
            tcx.codegen_select_candidate(typing_env.as_query_input(trait_ref))
        {
            source
        } else if still_specializable(tcx, typing_env.param_env, trait_item_def_id, trait_ref, None)
        {
            return TraitResolved::UnknownNotFound;
        } else {
            return TraitResolved::NoInstance;
        };
        trace!("TraitResolved::resolve {source:?}",);

        match source {
            ImplSource::UserDefined(impl_data) => {
                if still_specializable(
                    tcx,
                    typing_env.param_env,
                    trait_item_def_id,
                    trait_ref,
                    Some(source),
                ) {
                    return TraitResolved::UnknownFound;
                }

                // Find the id of the actual associated method we will be running
                let leaf_def = tcx
                    .trait_def(trait_ref.def_id)
                    .ancestors(tcx, impl_data.impl_def_id)
                    .unwrap()
                    .leaf_def(tcx, trait_item_def_id)
                    .unwrap_or_else(|| {
                        panic!("{:?} not found in {:?}", trait_item_def_id, impl_data.impl_def_id);
                    });

                // Translate the original substitution into one on the selected impl method
                let infcx = tcx.infer_ctxt().build(TypingMode::non_body_analysis());

                let args = rustc_trait_selection::traits::translate_args(
                    &infcx,
                    typing_env.param_env,
                    impl_data.impl_def_id,
                    impl_data.args,
                    leaf_def.defining_node,
                );
                let substs = substs.rebase_onto(tcx, trait_ref.def_id, args);

                let leaf_substs = infcx.tcx.erase_regions(substs);

                TraitResolved::Instance(leaf_def.item.def_id, leaf_substs)
            }
            ImplSource::Param(_) => TraitResolved::UnknownFound,
            ImplSource::Builtin(_, _) => match *substs.type_at(0).kind() {
                rustc_middle::ty::Closure(closure_def_id, closure_substs) => {
                    TraitResolved::Instance(closure_def_id, closure_substs)
                }
                _ => unimplemented!(),
            },
        }
    }

    /// Given a trait and some type parameters, try to find a concrete `impl` block for
    /// this trait.
    pub(crate) fn impl_id_of_trait(
        tcx: TyCtxt<'tcx>,
        typing_env: TypingEnv<'tcx>,
        trait_def_id: DefId,
        substs: GenericArgsRef<'tcx>,
    ) -> Option<DefId> {
        let trait_ref = TraitRef::from_method(tcx, trait_def_id, substs);
        let trait_ref = tcx.normalize_erasing_regions(typing_env, trait_ref);

        let Ok(source) = tcx.codegen_select_candidate(typing_env.as_query_input(trait_ref)) else {
            return None;
        };
        trace!("TraitResolved::impl_id_of_trait {source:?}",);
        match source {
            ImplSource::UserDefined(impl_data) => Some(impl_data.impl_def_id),
            ImplSource::Param(_) => None,
            // TODO: should we return something here, like we do in the above method?
            ImplSource::Builtin(_, _) => None,
        }
    }

    pub fn to_opt(
        self,
        did: DefId,
        substs: GenericArgsRef<'tcx>,
    ) -> Option<(DefId, GenericArgsRef<'tcx>)> {
        match self {
            TraitResolved::Instance(did, substs) => Some((did, substs)),
            TraitResolved::NotATraitItem | TraitResolved::UnknownFound => Some((did, substs)),
            _ => None,
        }
    }
}

fn instantiate_params_with_infer<'tcx, T: TypeFoldable<TyCtxt<'tcx>>>(
    ctx: &InferCtxt<'tcx>,
    value: T,
) -> T {
    struct Folder<'a, 'tcx> {
        ctx: &'a InferCtxt<'tcx>,
        tys: HashMap<ParamTy, Ty<'tcx>>,
        consts: HashMap<ParamConst, Const<'tcx>>,
    }
    impl<'tcx> TypeFolder<TyCtxt<'tcx>> for Folder<'_, 'tcx> {
        fn cx(&self) -> TyCtxt<'tcx> {
            self.ctx.tcx
        }

        fn fold_ty(&mut self, t: Ty<'tcx>) -> Ty<'tcx> {
            match *t.kind() {
                TyKind::Param(param) => {
                    *self.tys.entry(param).or_insert_with(|| self.ctx.next_ty_var(DUMMY_SP))
                }
                _ => t.super_fold_with(self),
            }
        }

        fn fold_const(&mut self, c: Const<'tcx>) -> Const<'tcx> {
            match c.kind() {
                ConstKind::Param(param) => {
                    *self.consts.entry(param).or_insert_with(|| self.ctx.next_const_var(DUMMY_SP))
                }
                _ => c.super_fold_with(self),
            }
        }
    }
    value.fold_with(&mut Folder { ctx, tys: Default::default(), consts: Default::default() })
}

fn still_specializable<'tcx>(
    tcx: TyCtxt<'tcx>,
    param_env: ParamEnv<'tcx>,
    trait_item_def_id: DefId,
    trait_ref: TraitRef<'tcx>,
    source: Option<&ImplSource<'tcx, ()>>,
) -> bool {
    let Ok(graph) = tcx.specialization_graph_of(trait_ref.def_id) else {
        // TODO: the proper way to do this would be to bubble the error up
        // so that we can continue for a bit, and maybe report other errors
        tcx.dcx().abort_if_errors();
        unreachable!()
    };

    // Search for the least specialized node that applies to this trait_ref
    let start_node = if let Some(ImplSource::UserDefined(ud)) = source {
        let trait_def = tcx.trait_def(trait_ref.def_id);
        let leaf = trait_def
            .ancestors(tcx, ud.impl_def_id)
            .unwrap()
            .leaf_def(tcx, trait_item_def_id)
            .unwrap();
        if !(leaf.item.defaultness(tcx).is_default()
            || tcx.defaultness(leaf.defining_node.def_id()).is_default())
        {
            // The leaf node is not marked as default => cannot be specialized
            return false;
        }

        leaf.defining_node.def_id()
    } else {
        trait_ref.def_id
    };

    // Check whether we know all the nodes.
    // We take inspiration from rustc_next_solver::cohenrence::trait_ref_is_knowable,
    // but ignore future-compatibility.
    let infcx = tcx.infer_ctxt().ignoring_regions().build(rustc_type_ir::TypingMode::Coherence);
    let (param_env, trait_ref) =
        instantiate_params_with_infer(&infcx, param_env.and(trait_ref)).into_parts();
    if orphan_check_trait_ref(&infcx, trait_ref, InCrate::Remote, |ty| Ok::<_, !>(ty))
        .unwrap()
        .is_ok()
    {
        // A downstream or cousin crate is allowed to implement some
        // generic parameters of this trait-ref.
        return true;
    }

    // Check wether one of the descendents of start_node applies too
    let def_children = Default::default();
    let get_children = |node| {
        let ch = graph.children.get(&node).unwrap_or(&def_children);
        let nonblanket = ch.non_blanket_impls.iter().flat_map(|(_, v)| v.iter());
        ch.blanket_impls.iter().chain(nonblanket).cloned().collect::<Vec<DefId>>()
    };

    let mut stack = get_children(start_node);
    while let Some(node) = stack.pop() {
        let infcx = infcx.fork();

        let args = infcx.fresh_args_for_item(DUMMY_SP, node);
        let trait_ref_node = tcx.impl_trait_ref(node).unwrap().instantiate(tcx, args);
        if infcx
            .at(&ObligationCause::dummy(), param_env)
            .eq(DefineOpaqueTypes::Yes, trait_ref_node, trait_ref)
            .is_err()
        {
            continue;
        }
        if tcx.impl_item_implementor_ids(node).get(&trait_item_def_id).is_some() {
            return true;
        }
        stack.extend(get_children(node));
    }

    false
}
