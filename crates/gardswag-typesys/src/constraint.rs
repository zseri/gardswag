use crate::{ArgMultiplicityId as ArgMultId, FreeVars, Substitutable, Symbol, Ty, TyVar};
use core::fmt;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use crate::collect::{
    ArgMultiplicity as ArgMult, Constraint, TyConstraintGroup as TyGroup,
    TyConstraintGroupKind as TyGroupKind,
};
use TyGroupKind as Tcgk;

#[derive(Debug, thiserror::Error)]
pub enum UnifyError {
    #[error("infinite type in {v:?} = {t:?}")]
    Infinite { v: TyVar, t: Ty },
    #[error("subtitution of {v:?} = {t1:?} overridden with {t2:?}")]
    Override { v: TyVar, t1: Ty, t2: Ty },
    #[error("unification of {t1:?} = {t2:?} failed")]
    Mismatch { t1: Ty, t2: Ty },
    #[error("unification of {c1:?} = {c2:?} failed")]
    MismatchConstrGroupKind { c1: Tcgk, c2: Tcgk },
    #[error("unification of {a1:?} = {a2:?} failed")]
    MismatchArgMultiplicity { a1: ArgMult, a2: ArgMult },

    #[error("got infinite constraint group while merging $c{c1:?} with $c{c2:?}")]
    InfiniteConstraintGroup {
        c1: TyConstraintGroupId,
        c2: TyConstraintGroupId,
    },

    #[error("partial row constraint failed while merging container type {container:?} and value type {value:} at key {key:?}")]
    Partial {
        key: Symbol,
        value: Ty,
        container: Ty,
    },

    #[error("one-of constraint conflict between {oo1:?} and {oo2:?} (intersection is empty)")]
    OneOfConflict { oo1: Vec<Ty>, oo2: Vec<Ty> },

    #[error("one-of constraint got {got:?}, but expected {expected:?}")]
    OneOfConstraint { got: Ty, expected: Vec<Ty> },

    #[error("expected arrow, got {0:?}")]
    NotAnArrow(Ty),

    #[error("expected record, got {0:?}")]
    NotARecord(Ty),

    #[error("expected tagged union, got {0:?}")]
    NotATaggedUnion(Ty),
}

type Tvs = BTreeMap<TyVar, TyConstraintGroupId>;

#[derive(Clone, Copy, Default, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(transparent)]
#[repr(transparent)]
pub struct TyConstraintGroupId(usize);

impl fmt::Debug for TyConstraintGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "$cg{}", self.0)
    }
}

impl fmt::Display for TyConstraintGroupId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "$cg{}", self.0)
    }
}

/// get lowest ty variable with same cgid (used for normalization)
fn lowest_tvi_for_cg(m: &Tvs, tv: TyVar) -> TyVar {
    if let Some(&x) = m.get(&tv) {
        // check if any tv with the same *x has a lower id
        // and replace it with that
        return *m.iter().find(|(_, &v)| v == x).unwrap().0;
    }
    tv
}

/// the option merge pattern, used when merging constraint groups
fn opt_merge<T, E, F>(a: Option<T>, b: Option<T>, f: F) -> Result<Option<T>, E>
where
    F: FnOnce(T, T) -> Result<T, E>,
{
    Ok(match (a, b) {
        (Some(a), Some(b)) => Some(f(a, b)?),
        (Some(x), None) | (None, Some(x)) => Some(x),
        (None, None) => None,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Context {
    pub a: BTreeMap<ArgMultId, ArgMult>,
    pub g: BTreeMap<TyConstraintGroupId, TyGroup>,
    pub m: Tvs,
    pub tycg_cnt: core::ops::RangeFrom<usize>,
}

impl Default for Context {
    fn default() -> Self {
        Self {
            a: Default::default(),
            g: Default::default(),
            m: Default::default(),
            tycg_cnt: 0..,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SchemeSer {
    pub forall: Vec<TyGroup>,
    pub forall_args: Vec<Option<ArgMult>>,
    pub ty: Ty,
}

impl SchemeSer {
    pub fn min_unused_tyvar(&self) -> usize {
        let mut tfv = BTreeSet::default();
        if let Some(x) = self.forall.len().checked_sub(1) {
            tfv.insert(x);
        }
        self.forall.fv(&mut tfv, true);
        self.ty.fv(&mut tfv, true);
        tfv.into_iter().rev().next().map(|x| x + 1).unwrap_or(0)
    }
}

impl Context {
    /// resolve the context using itself as far as possible
    pub fn self_resolve(&mut self) -> Result<(), UnifyError> {
        loop {
            let notify_cgs = self
                .g
                .values()
                .flat_map(|i| &i.listeners)
                .flat_map(|i| self.m.get(i))
                .copied()
                .collect();
            self.notify_cgs(notify_cgs)?;
            let mut newg = self.g.clone();
            newg.apply(&mut |&i| self.on_apply(i));
            if newg == self.g {
                break Ok(());
            }
            self.g = newg;
        }
    }

    pub fn on_apply(&self, i: TyVar) -> Option<Ty> {
        let cgid = *self.m.get(&i)?;
        let span = tracing::span!(tracing::Level::TRACE, "on_apply", %i);
        let _enter = span.enter();
        let j = lowest_tvi_for_cg(&self.m, i);
        let ret = self.g.get(&cgid).and_then(|k| k.ty.clone()).map(|mut k| {
            k.apply(&mut |&l| {
                if l == i {
                    tracing::error!(
                        "on_apply: unmarked infinite type warped at ${} = {:?}",
                        i,
                        self.g.get(&cgid).unwrap().ty.clone()
                    );
                    panic!("on_apply: unmarked infinite type warped at ${}", i);
                } else {
                    self.on_apply(l)
                }
            });
            k
        });
        tracing::trace!(%j, "{:?}", ret);
        Some(if let Some(x) = ret { x } else { Ty::Var(j) })
    }

    pub fn min_unused_tyvar(&self) -> usize {
        let mut tfv = BTreeSet::default();
        if let Some(x) = self.m.keys().last() {
            tfv.insert(*x);
        }
        self.g.fv(&mut tfv, true);
        tfv.into_iter().rev().next().map(|x| x + 1).unwrap_or(0)
    }

    fn min_unused_argmultid(&self) -> ArgMultId {
        let mut afv = BTreeSet::default();
        if let Some(x) = self.a.keys().last() {
            afv.insert(*x);
        }
        self.g.fv(&mut afv, true);
        self.a.fv(&mut afv, true);
        ArgMultId(
            afv.into_iter()
                .rev()
                .next()
                .map(|ArgMultId(x)| x + 1)
                .unwrap_or(0),
        )
    }

    fn tyvar_closure(&self, closure: &mut BTreeSet<TyVar>) {
        // create proper closure of type variables
        loop {
            let orig_len = closure.len();
            let mut new_closure = BTreeSet::<TyVar>::new();
            for i in &*closure {
                if let Some(x) = self.m.get(i).and_then(|j| self.g.get(j)) {
                    assert!(x.resolved().is_none());
                    x.fv(&mut new_closure, true);
                }
            }
            closure.extend(new_closure);
            if orig_len == closure.len() {
                break;
            }
        }

        // minimize closure
        *closure = core::mem::take(closure)
            .into_iter()
            .map(|i| lowest_tvi_for_cg(&self.m, i))
            .collect();
    }

    fn argmulti_closure(&self, closure: &mut BTreeSet<ArgMultId>) {
        // create proper closure of argmulti variables
        loop {
            let orig_len = closure.len();
            let mut new_closure = BTreeSet::<ArgMultId>::new();
            for i in &*closure {
                if let Some(x) = self.a.get(i) {
                    x.fv(&mut new_closure, true);
                }
            }
            closure.extend(new_closure);
            if orig_len == closure.len() {
                break;
            }
        }
    }

    /// exports a type scheme into a de/serializable form
    /// please make sure to only call this after [`self_resolve`](Context::self_resolve)
    /// and before any subsequent mutating operations
    pub fn export_scheme(&self, crate::Scheme { forall, mut ty }: crate::Scheme) -> SchemeSer {
        ty.apply(&mut |&i| self.on_apply(i));

        let mut closure = forall;
        self.tyvar_closure(&mut closure);

        // make sure scheme is honest about it's minimal dependencies,
        // the `ty.apply` above worked correctly and coalesced the tyvars
        // and does not contain any external unresolved dependencies.
        {
            let mut min_deps = BTreeSet::new();
            ty.fv(&mut min_deps, true);
            if let Some(x) = min_deps.difference(&closure).next() {
                panic!("scheme dependencies are inconsistent or unresolved: {:?} is a tyvar dependency, but not in forall", x);
            }
        }

        tracing::debug!(?closure, ?ty, "export_scheme");

        // create mapping (remap forall items to be consecutive)
        let mut m = BTreeMap::new();
        let mut forall = Vec::new();
        for (n, (i, k)) in closure
            .into_iter()
            .map(|i| (i, self.m.get(&i).and_then(|j| self.g.get(j).cloned())))
            .enumerate()
        {
            m.insert(i, n);
            forall.push(k.unwrap_or_default());
        }
        tracing::debug!("export_scheme tyvar mapping: {:?}", m);

        // create mapping for argmulti's
        let mut am = BTreeMap::new();
        let mut forall_args = Vec::new();
        let mut afv = BTreeSet::<ArgMultId>::new();
        forall.fv(&mut afv, true);
        self.argmulti_closure(&mut afv);
        for (n, i) in afv.into_iter().enumerate() {
            am.insert(i, n);
            forall_args.push(self.a.get(&i).cloned());
        }

        // apply mapping
        let mut my_on_apply = |j: &TyVar| Some(Ty::Var(*m.get(j).unwrap()));
        let mut my_on_apply_a = |j: &ArgMultId| Some(ArgMultId(*am.get(j).unwrap()));
        for k in &mut forall {
            k.apply(&mut my_on_apply);
            k.apply(&mut my_on_apply_a);
        }
        ty.apply(&mut my_on_apply);

        SchemeSer {
            forall,
            forall_args,
            ty,
        }
    }

    /// imports a (potentially serialized) type scheme
    pub fn import_scheme(
        &mut self,
        SchemeSer {
            forall,
            forall_args,
            mut ty,
        }: SchemeSer,
    ) -> crate::Scheme {
        let mut am = BTreeMap::new();
        let ArgMultId(mut new_an) = self.min_unused_argmultid();
        for (n, i) in forall_args.into_iter().enumerate() {
            let amid = ArgMultId(new_an);
            if let Some(j) = i {
                self.a.insert(amid, j);
            }
            am.insert(ArgMultId(n), amid);
            new_an += 1;
        }

        let mut m = BTreeMap::new();
        let mut new_n = self.min_unused_tyvar();

        for (n, i) in forall.into_iter().enumerate() {
            let tcgid = TyConstraintGroupId(self.tycg_cnt.next().unwrap());
            self.g.insert(tcgid, i);
            self.m.insert(new_n, tcgid);
            m.insert(n, new_n);
            new_n += 1;
        }

        let mut my_on_apply = |j: &TyVar| {
            Some(Ty::Var(*m.get(j).unwrap_or_else(|| {
                panic!("unknown type variable ${} in SchemeSer", j)
            })))
        };
        let mut my_on_apply_a = |j: &ArgMultId| {
            Some(
                *am.get(j)
                    .unwrap_or_else(|| panic!("unknown argmulti variable {} in SchemeSer", j)),
            )
        };

        for i in am.values() {
            self.a.get_mut(i).unwrap().apply(&mut my_on_apply_a);
        }

        for i in m.values() {
            let tcgid = *self.m.get(i).unwrap();
            let gm = self.g.get_mut(&tcgid).unwrap();
            gm.apply(&mut my_on_apply);
            gm.apply(&mut my_on_apply_a);
        }

        ty.apply(&mut my_on_apply);

        crate::Scheme {
            forall: m.values().copied().collect(),
            ty,
        }
    }

    pub fn solve(&mut self, constrs: crate::collect::Context) -> Result<(), (usize, UnifyError)> {
        use core::mem::take;
        let mut constraints = constrs.constraints;
        for (offset, constr) in take(&mut constraints) {
            tracing::trace!("***solve*** @{} {:?}", offset, constr);
            let tmp = match constr {
                Constraint::Unify(a, b) => self.unify(&a, &b),
                Constraint::Bind(tv, cg) => self.bind(tv, cg),
                Constraint::BindArgMulti(amid, amdat) => self.bind_argmulti(amid, amdat),
            };
            match tmp {
                Ok(()) => {}
                Err(e) => return Err((offset, e)),
            }
        }
        Ok(())
    }

    fn unify(&mut self, a: &Ty, b: &Ty) -> Result<(), UnifyError> {
        //tracing::trace!(%a, %b, ?self, "unify");
        // self clutters the output too much
        tracing::trace!(%a, %b, "unify");
        match (a, b) {
            (
                Ty::Arrow {
                    arg_multi: am1,
                    arg: arg1,
                    ret: ret1,
                },
                Ty::Arrow {
                    arg_multi: am2,
                    arg: arg2,
                    ret: ret2,
                },
            ) if am1 == am2 => {
                self.unify(arg1, arg2)?;
                let (mut rx1, mut rx2) = (ret1.clone(), ret2.clone());
                rx1.apply(&mut |&i| self.on_apply(i));
                rx2.apply(&mut |&i| self.on_apply(i));
                self.unify(&rx1, &rx2)?;
                Ok(())
            }
            (Ty::Record(rc1), Ty::Record(rc2)) if rc1.len() == rc2.len() => {
                for (k, v1) in rc1 {
                    let v2 = rc2.get(k).ok_or_else(|| UnifyError::Mismatch {
                        t1: a.clone(),
                        t2: b.clone(),
                    })?;
                    self.unify(v1, v2)?;
                }
                Ok(())
            }
            (Ty::TaggedUnion(tu1), Ty::TaggedUnion(tu2)) if tu1.len() == tu2.len() => {
                for (k, v1) in tu1 {
                    let v2 = tu2.get(k).ok_or_else(|| UnifyError::Mismatch {
                        t1: a.clone(),
                        t2: b.clone(),
                    })?;
                    self.unify(v1, v2)?;
                }
                Ok(())
            }
            (Ty::ChanSend(x), Ty::ChanSend(y)) => self.unify(x, y),
            (Ty::ChanRecv(x), Ty::ChanRecv(y)) => self.unify(x, y),
            (Ty::Var(a), Ty::Var(b)) => {
                let tcgid = match (self.m.get(a), self.m.get(b)) {
                    (None, None) => {
                        let tcgid = TyConstraintGroupId(self.tycg_cnt.next().unwrap());
                        let tmp = self.g.insert(tcgid, Default::default());
                        assert_eq!(tmp, None);
                        tcgid
                    }
                    (Some(&tcgid), None) | (None, Some(&tcgid)) => tcgid,
                    (Some(&vcg), Some(&ycg)) => return self.unify_constraint_groups(vcg, ycg),
                };
                self.m.insert(*a, tcgid);
                self.m.insert(*b, tcgid);
                Ok(())
            }
            (Ty::Var(a), t) | (t, Ty::Var(a)) => self.bind(
                *a,
                TyGroup {
                    ty: Some(t.clone()),
                    ..Default::default()
                },
            ),
            (Ty::Literal(l), Ty::Literal(r)) if l == r => Ok(()),
            (_, _) => Err(UnifyError::Mismatch {
                t1: a.clone(),
                t2: b.clone(),
            }),
        }
    }

    fn ucg_check4inf(
        &self,
        a: TyConstraintGroupId,
        b: TyConstraintGroupId,
        t: &Ty,
    ) -> Result<(), UnifyError> {
        let mut fvaccu = Default::default();
        t.fv(&mut fvaccu, true);
        for i in fvaccu {
            if let Some(x) = self.m.get(&i) {
                let x = *x;
                if x == a {
                    return Err(UnifyError::InfiniteConstraintGroup { c1: a, c2: b });
                }
                debug_assert_ne!(x, b);
            }
        }
        Ok(())
    }

    fn unify_cgk_and_ty(&mut self, tcgk: &mut Tcgk, ty: &Ty) -> Result<(), UnifyError> {
        if let Ty::Var(_) = ty {
            return Ok(());
        }
        match tcgk {
            Tcgk::Arrow {
                multi,
                ty_arg,
                ty_ret,
            } => {
                if let Ty::Arrow {
                    arg_multi,
                    arg,
                    ret,
                } = ty
                {
                    self.bind_argmulti(*multi, (*arg_multi).into())?;
                    self.unify(ty_arg, arg)?;
                    self.unify(ty_ret, ret)?;
                } else {
                    return Err(UnifyError::NotAnArrow(ty.clone()));
                }
            }
            Tcgk::Record { partial, .. } => {
                if let Ty::Record(rcm) = ty {
                    for (key, value) in core::mem::take(partial) {
                        if let Some(got_valty) = rcm.get(&key) {
                            self.unify(got_valty, &value)?;
                        } else {
                            return Err(UnifyError::Partial {
                                key,
                                value,
                                container: ty.clone(),
                            });
                        }
                    }
                } else {
                    return Err(UnifyError::NotARecord(ty.clone()));
                }
            }
            Tcgk::TaggedUnion { partial, .. } => {
                if let Ty::TaggedUnion(tum) = ty {
                    for (key, value) in core::mem::take(partial) {
                        if let Some(got_valty) = tum.get(&key) {
                            self.unify(got_valty, &value)?;
                        } else {
                            return Err(UnifyError::Partial {
                                key,
                                value,
                                container: ty.clone(),
                            });
                        }
                    }
                } else {
                    return Err(UnifyError::NotATaggedUnion(ty.clone()));
                }
            }
        }
        Ok(())
    }

    fn notify_cgs(&mut self, mut cgs: BTreeSet<TyConstraintGroupId>) -> Result<(), UnifyError> {
        loop {
            let cg = {
                let mut it = core::mem::take(&mut cgs).into_iter();
                let cg = match it.next() {
                    Some(x) => x,
                    None => break,
                };
                cgs = it.collect();
                cg
            };
            tracing::trace!(?cg, "notify-cg");
            let mut g = self.g.remove(&cg).unwrap();
            if g.resolved().is_some() {
                tracing::trace!(?cg, ?g, "already resolved");
                self.g.insert(cg, g);
                // nothing to do
                return Ok(());
            }

            // recheck constraints
            // buffer notifications to prevent unnecessary infinite loops
            let mut modified = false;

            if let Some(Tcgk::Record {
                update_info,
                partial,
            }) = &mut g.kind
            {
                if let Some((orig, ovrd)) = update_info {
                    match (
                        self.g.get(self.m.get(orig).unwrap()),
                        self.g.get(self.m.get(ovrd).unwrap()),
                    ) {
                        (
                            Some(TyGroup {
                                ty: Some(Ty::Record(rcm_orig)),
                                ..
                            }),
                            Some(TyGroup {
                                ty: Some(Ty::Record(rcm_ovrd)),
                                ..
                            }),
                        ) => {
                            let mut rcm = rcm_ovrd.clone();
                            for (k, v) in rcm_orig {
                                if rcm.contains_key(k) {
                                    continue;
                                }
                                rcm.insert(*k, v.clone());
                            }
                            let rcm_ty = Ty::Record(rcm);
                            if let Some(ty) = &g.ty {
                                self.unify(&rcm_ty, ty)?;
                            }
                            modified = true;
                            *update_info = None;
                            g.ty = Some(rcm_ty);
                        }
                        (
                            Some(TyGroup {
                                ty:
                                    Some(
                                        ty_orig @ (Ty::Literal(_)
                                        | Ty::Arrow { .. }
                                        | Ty::TaggedUnion(_)
                                        | Ty::ChanSend(_)
                                        | Ty::ChanRecv(_)),
                                    ),
                                ..
                            }),
                            _,
                        ) => {
                            return Err(UnifyError::NotARecord(ty_orig.clone()));
                        }
                        (
                            _,
                            Some(TyGroup {
                                ty:
                                    Some(
                                        ty_ovrd @ (Ty::Literal(_)
                                        | Ty::Arrow { .. }
                                        | Ty::TaggedUnion(_)
                                        | Ty::ChanSend(_)
                                        | Ty::ChanRecv(_)),
                                    ),
                                ..
                            }),
                        ) => {
                            return Err(UnifyError::NotARecord(ty_ovrd.clone()));
                        }
                        (
                            _,
                            Some(TyGroup {
                                ty: Some(Ty::Record(rcm_ovrd)),
                                ..
                            }),
                        ) => {
                            // if an item is present in the override, we can already propagate it
                            let mut unifiers = Vec::new();
                            for (k, v) in core::mem::take(partial) {
                                match rcm_ovrd.get(&k).cloned() {
                                    Some(v2) => {
                                        unifiers.push((v, v2));
                                    }
                                    None => {
                                        partial.insert(k, v);
                                    }
                                }
                            }
                            for (v1, v2) in unifiers {
                                self.unify(&v1, &v2)?;
                            }
                        }
                        (
                            None
                            | Some(TyGroup {
                                ty: None | Some(Ty::Var(_)),
                                ..
                            }),
                            _,
                        )
                        | (
                            _,
                            None
                            | Some(TyGroup {
                                ty: None | Some(Ty::Var(_)),
                                ..
                            }),
                        ) => {}
                    }
                }
            }

            if let Some(ty) = &mut g.ty {
                ty.apply(&mut |&i| self.on_apply(i));
                let tfv = {
                    let mut tfv = Default::default();
                    ty.fv(&mut tfv, true);
                    tfv
                };
                if tfv.is_empty() {
                    let mut success = g.oneof.is_empty();
                    for j in &g.oneof {
                        let mut self_bak = self.clone();
                        if self_bak.unify(ty, j).is_ok() {
                            *self = self_bak;
                            success = true;
                            ty.apply(&mut |&i| self.on_apply(i));
                            break;
                        }
                    }
                    if !success {
                        return Err(UnifyError::OneOfConstraint {
                            expected: core::mem::take(&mut g.oneof),
                            got: ty.clone(),
                        });
                    }
                }

                if let Some(tcgk) = &mut g.kind {
                    self.unify_cgk_and_ty(tcgk, ty)?;
                }
            }

            if modified {
                cgs.extend(g.listeners.iter().flat_map(|i| self.m.get(i)).copied());
            }
            let tmp = self.g.insert(cg, g);
            assert_eq!(tmp, None);
        }
        Ok(())
    }

    fn unify_constraint_groups(
        &mut self,
        a: TyConstraintGroupId,
        b: TyConstraintGroupId,
    ) -> Result<(), UnifyError> {
        if a == b {
            return Ok(());
        }

        tracing::debug!("unify-cgs {} {}", a, b);
        let mut lhs = self.g.remove(&a).unwrap();
        let mut rhs = self.g.remove(&b).unwrap();

        // replace all occurences of $`b` with $`a`
        self.m.values_mut().filter(|i| **i == b).for_each(|i| {
            *i = a;
        });

        if lhs == rhs {
            self.g.insert(a, lhs);
            return Ok(());
        }

        let mut res = match (lhs.resolved(), rhs.resolved()) {
            (Some(t_a), Some(t_b)) => {
                tracing::trace!(?t_a, ?t_b, "unify-cgs");
                self.ucg_check4inf(a, b, t_a)?;
                self.ucg_check4inf(a, b, t_b)?;
                self.unify(t_a, t_b)?;
                lhs.ty.as_mut().unwrap().apply(&mut |&i| self.on_apply(i));
                debug_assert!({
                    rhs.ty.as_mut().unwrap().apply(&mut |&i| self.on_apply(i));
                    tracing::trace!(?lhs.ty, ?rhs.ty, "unify res");
                    lhs.ty == rhs.ty
                });
                lhs
            }
            (None, None) => {
                tracing::trace!(?lhs, ?rhs, "unify-cgs (full merge)");
                let mut ty = match (lhs.ty, rhs.ty) {
                    (None, None) => None,
                    (Some(t), None) | (None, Some(t)) => Some(t),
                    (Some(mut t1), Some(t2)) => {
                        self.unify(&t1, &t2)?;
                        t1.apply(&mut |&i| self.on_apply(i));
                        Some(t1)
                    }
                };
                tracing::trace!(?ty, "unify-cgs type");
                let listeners: BTreeSet<_> = lhs
                    .listeners
                    .into_iter()
                    .chain(rhs.listeners.into_iter())
                    .filter(|i| {
                        if let Some(&j) = self.m.get(i) {
                            j != a && j != b
                        } else {
                            false
                        }
                    })
                    .collect();

                let mut oneof: Vec<_> = lhs
                    .oneof
                    .iter()
                    .flat_map(|i| rhs.oneof.iter().find(|&j| i == j).map(|_| i.clone()))
                    .collect();
                if oneof.is_empty() && (!lhs.oneof.is_empty() || !rhs.oneof.is_empty()) {
                    return Err(UnifyError::OneOfConflict {
                        oo1: lhs.oneof,
                        oo2: rhs.oneof,
                    });
                }
                if oneof.len() == 1 {
                    let ty2 = oneof.remove(0);
                    if let Some(ty) = &mut ty {
                        self.unify(ty, &ty2)?;
                        ty.apply(&mut |&i| self.on_apply(i));
                    } else {
                        ty = Some(ty2.clone());
                    }
                }
                lhs.oneof.clear();
                rhs.oneof.clear();

                let kind = opt_merge(lhs.kind, rhs.kind, |lhs, rhs| match (lhs, rhs) {
                    (
                        Tcgk::Arrow {
                            multi: lhs_multi,
                            ty_arg: lhs_ty_arg,
                            ty_ret: lhs_ty_ret,
                        },
                        Tcgk::Arrow {
                            multi: rhs_multi,
                            ty_arg: rhs_ty_arg,
                            ty_ret: rhs_ty_ret,
                        },
                    ) => {
                        self.unify(&lhs_ty_arg, &rhs_ty_arg)?;
                        self.unify(&lhs_ty_ret, &rhs_ty_ret)?;
                        let (mut ty_arg, mut ty_ret) = (lhs_ty_arg, lhs_ty_ret);
                        ty_arg.apply(&mut |&i| self.on_apply(i));
                        ty_ret.apply(&mut |&i| self.on_apply(i));
                        let multi = if lhs_multi != rhs_multi {
                            let min_multi = ArgMultId(core::cmp::min(lhs_multi.0, rhs_multi.0));
                            let max_multi = ArgMultId(core::cmp::max(lhs_multi.0, rhs_multi.0));
                            let mut on_apply = |i: &ArgMultId| {
                                if *i == lhs_multi || *i == rhs_multi {
                                    Some(min_multi)
                                } else {
                                    None
                                }
                            };
                            self.a.apply(&mut on_apply);
                            self.g.apply(&mut on_apply);
                            if let Some(max_multi) = self.a.remove(&max_multi) {
                                self.bind_argmulti(min_multi, max_multi)?;
                            }
                            min_multi
                        } else {
                            lhs_multi
                        };
                        Ok(Tcgk::Arrow {
                            multi,
                            ty_arg,
                            ty_ret,
                        })
                    }
                    (
                        Tcgk::Record {
                            partial: lhs_partial,
                            update_info: lhs_update_info,
                        },
                        Tcgk::Record {
                            partial: rhs_partial,
                            update_info: rhs_update_info,
                        },
                    ) => {
                        let mut partial = lhs_partial;

                        if partial.is_empty() {
                            partial = rhs_partial;
                        } else {
                            for (key, value) in rhs_partial {
                                use std::collections::btree_map::Entry;
                                match partial.entry(key) {
                                    Entry::Occupied(mut occ) => {
                                        self.unify(occ.get(), &value)?;
                                        occ.get_mut().apply(&mut |&i| self.on_apply(i));
                                    }
                                    Entry::Vacant(vac) => {
                                        vac.insert(value);
                                    }
                                }
                            }
                        }

                        let update_info =
                            opt_merge(lhs_update_info, rhs_update_info, |(w, x), (y, z)| {
                                use Ty::Var;
                                self.unify(&Var(w), &Var(y))?;
                                self.unify(&Var(x), &Var(z))?;
                                Ok((lowest_tvi_for_cg(&self.m, w), lowest_tvi_for_cg(&self.m, x)))
                            })?;

                        Ok(Tcgk::Record {
                            partial,
                            update_info,
                        })
                    }
                    (
                        Tcgk::TaggedUnion {
                            partial: lhs_partial,
                        },
                        Tcgk::TaggedUnion {
                            partial: rhs_partial,
                        },
                    ) => {
                        let mut partial = lhs_partial;

                        if partial.is_empty() {
                            partial = rhs_partial;
                        } else {
                            for (key, value) in rhs_partial {
                                use std::collections::btree_map::Entry;
                                match partial.entry(key) {
                                    Entry::Occupied(mut occ) => {
                                        self.unify(occ.get(), &value)?;
                                        occ.get_mut().apply(&mut |&i| self.on_apply(i));
                                    }
                                    Entry::Vacant(vac) => {
                                        vac.insert(value);
                                    }
                                }
                            }
                        }

                        Ok(Tcgk::TaggedUnion { partial })
                    }
                    (lhs, rhs) => Err(UnifyError::MismatchConstrGroupKind { c1: lhs, c2: rhs }),
                })?;

                TyGroup {
                    ty,
                    oneof,
                    listeners,
                    kind,
                }
            }
            (_, _) => {
                let (t, mut g) = if let Some(t) = lhs.resolved() {
                    (t, rhs)
                } else {
                    (rhs.ty.as_ref().unwrap(), lhs)
                };
                tracing::trace!(?t, ?g, "unify-cg");
                self.ucg_check4inf(a, b, t)?;
                let tfv = {
                    let mut tfv = Default::default();
                    t.fv(&mut tfv, true);
                    tfv
                };
                if !tfv.is_empty() {
                    for i in tfv {
                        self.ucg_check4inf(a, b, &Ty::Var(i))?;
                    }
                } else if !g.oneof.is_empty() {
                    if g.oneof.len() == 1 {
                        let j = core::mem::take(&mut g.oneof).into_iter().next().unwrap();
                        self.ucg_check4inf(a, b, &j)?;
                        self.unify(t, &j)?;
                    } else {
                        let mut success = false;
                        for j in &g.oneof {
                            let mut self_bak = self.clone();
                            if self_bak.unify(t, j).is_ok() {
                                *self = self_bak;
                                success = true;
                                break;
                            }
                        }
                        if !success {
                            return Err(UnifyError::OneOfConstraint {
                                expected: core::mem::take(&mut g.oneof),
                                got: t.clone(),
                            });
                        }
                    }
                }
                if let Some(tcgk) = &mut g.kind {
                    self.unify_cgk_and_ty(tcgk, t)?;
                }
                if let Some(old) = &g.ty {
                    self.unify(old, t)?;
                } else {
                    g.ty = Some(t.clone());
                }
                g
            }
        };
        res.apply(&mut |&i| self.on_apply(i));
        let notify_cgs = res
            .listeners
            .iter()
            .flat_map(|i| self.m.get(i))
            .copied()
            .collect();

        let x = self.g.insert(a, res);
        assert_eq!(x, None);
        // propagate inference information
        self.notify_cgs(notify_cgs)?;
        Ok(())
    }

    fn bind(&mut self, v: TyVar, tcg: TyGroup) -> Result<(), UnifyError> {
        if let Some(t) = tcg.resolved() {
            if let Ty::Var(y) = t {
                return self.unify(&Ty::Var(v), &Ty::Var(*y));
            }
            let tfv = {
                let mut tfv = Default::default();
                t.fv(&mut tfv, true);
                tfv
            };
            if tfv.contains(&v) {
                return Err(UnifyError::Infinite { v, t: t.clone() });
            }
        }
        use std::collections::btree_map::Entry;
        // lazy group allocation
        fn rhs_tcgid(
            tycg_cnt: &mut core::ops::RangeFrom<usize>,
            g: &mut BTreeMap<TyConstraintGroupId, TyGroup>,
            v: TyVar,
            tcg: TyGroup,
        ) -> TyConstraintGroupId {
            let rhs_tcgid = TyConstraintGroupId(tycg_cnt.next().unwrap());
            let z = g.insert(rhs_tcgid, tcg);
            assert_eq!(z, None);
            tracing::debug!("bound ${} <- {}", v, rhs_tcgid);
            rhs_tcgid
        }
        match self.m.entry(v) {
            Entry::Occupied(occ) => {
                let lhs_tcgid = *occ.get();
                if let Some(lhs_ty) = self.g.get(&lhs_tcgid).unwrap().resolved() {
                    // avoid unnecessary allocation of tcgid
                    if let Some(rhs_ty) = tcg.ty {
                        let lhs_ty = lhs_ty.clone();
                        return self.unify(&lhs_ty, &rhs_ty);
                    }
                }
                let rhs_tcgid = rhs_tcgid(&mut self.tycg_cnt, &mut self.g, v, tcg);
                self.unify_constraint_groups(lhs_tcgid, rhs_tcgid)
            }
            Entry::Vacant(y) => {
                let rhs_tcgid = rhs_tcgid(&mut self.tycg_cnt, &mut self.g, v, tcg);
                y.insert(rhs_tcgid);
                if self.g.values().flat_map(|i| &i.listeners).any(|&i| i == v) {
                    self.notify_cgs(core::iter::once(rhs_tcgid).collect())?;
                }
                Ok(())
            }
        }
    }

    fn bind_argmulti(&mut self, v: ArgMultId, dat: ArgMult) -> Result<(), UnifyError> {
        use std::collections::btree_map::Entry;
        match self.a.entry(v) {
            Entry::Occupied(occ) => {
                let mut occ = occ.into_mut();
                match (&mut occ, dat) {
                    (ArgMult::Linear, ArgMult::Linear) => return Ok(()),
                    (ArgMult::Unrestricted, ArgMult::Unrestricted) => return Ok(()),
                    (ArgMult::Sum(xs), ArgMult::Sum(mut ys)) => {
                        xs.sort();
                        ys.sort();
                        tracing::debug!("bind_argmulti: sums {:?} & {:?}", xs, ys);
                        if *xs != ys {
                            *occ = ArgMult::Unrestricted;
                        }
                        /*
                        let mut zs = Vec::new();
                        let mut yit = ys.into_iter().peekable();
                        for i in &xs {
                            if Some(i) == yit.peek() {
                                yit.next();
                            } else {
                                // ???
                            }
                        }
                        xs.extend(zs);
                        xs.sort();
                        */
                    }
                    (ArgMult::Max(xs), ArgMult::Max(ys)) => {
                        tracing::debug!("bind_argmulti: maxis {:?} & {:?}", xs, ys);
                        xs.extend(ys);
                    }
                    (ArgMult::Prod(xs), ArgMult::Prod(ys)) => {
                        tracing::debug!("bind_argmulti: products {:?} & {:?}", xs, ys);
                        xs.extend(ys);
                    }
                    (occ, dat) => {
                        return Err(UnifyError::MismatchArgMultiplicity {
                            a1: occ.clone(),
                            a2: dat,
                        })
                    }
                }
            }
            Entry::Vacant(vac) => {
                vac.insert(dat);
            }
        }
        // TODO: this is probably way too costly,
        // and should be only done once,
        // but we would need to annotate more stuff with
        // location info to be able to have some sort of error <-> location bundling.
        self.notify_argmultis()
    }

    pub fn notify_argmultis(&mut self) -> Result<(), UnifyError> {
        use crate::FinalArgMultiplicity as Fam;
        fn resolve_argmulti(
            v: &mut ArgMult,
            finm: &BTreeMap<ArgMultId, Fam>,
            ign: ArgMultId,
        ) -> Option<Fam> {
            v.normalize();
            let ret = match v {
                ArgMult::Linear => Fam::Linear,
                ArgMult::Unrestricted => Fam::Unrestricted,
                ArgMult::Var(i) => *finm.get(i)?,
                ArgMult::Sum(xs) => {
                    let mut sum = Fam::Erased;
                    // remove all elements which are zero
                    xs.retain(|i| match i {
                        ArgMult::Sum(i) if i.is_empty() => false,
                        ArgMult::Var(j) if finm.get(j) == Some(&Fam::Erased) => false,
                        _ => true,
                    });
                    for i in xs {
                        if *i == ArgMult::Var(ign) {
                            tracing::debug!("notify_argmultis ignored loop around {:?}", ign);
                            continue;
                        }
                        sum = match (resolve_argmulti(i, finm, ign)?, sum) {
                            (Fam::Erased, x) | (x, Fam::Erased) => x,
                            (_, _) => Fam::Unrestricted,
                        };
                    }
                    sum
                }
                ArgMult::Max(xs) => {
                    let mut max = None;
                    for i in xs {
                        if *i == ArgMult::Var(ign) {
                            tracing::debug!("notify_argmultis ignored loop around {:?}", ign);
                            continue;
                        }
                        let j = resolve_argmulti(i, finm, ign)?;
                        max = Some(match max {
                            // we can't assume anything if any branch mismatches
                            Some(k) if k != j => Fam::Unrestricted,
                            None | Some(_) => j,
                        });
                    }
                    max.expect("max was empty")
                }
                ArgMult::Prod(xs) => {
                    let mut prod = Fam::Linear;
                    // remove all elements which are one
                    xs.retain(|i| i != &ArgMult::Linear);
                    for i in xs {
                        if *i == ArgMult::Var(ign) {
                            tracing::debug!("notify_argmultis ignored loop around {:?}", ign);
                            continue;
                        }
                        prod = match (prod, resolve_argmulti(i, finm, ign)?) {
                            (Fam::Erased, _) | (_, Fam::Erased) => Fam::Erased,
                            (Fam::Linear, x) | (x, Fam::Linear) => x,
                            (_, _) => Fam::Unrestricted,
                        };
                    }
                    prod
                }
            };
            *v = match ret {
                Fam::Erased => ArgMult::Sum(Vec::new()),
                Fam::Linear => ArgMult::Linear,
                Fam::Unrestricted => ArgMult::Unrestricted,
            };
            Some(ret)
        }

        tracing::trace!("notify_argmultis");

        // scanner run
        let mut finm: BTreeMap<_, _> = self
            .a
            .iter()
            .flat_map(|(k, i)| {
                match i {
                    ArgMult::Linear => Some(Fam::Linear),
                    ArgMult::Unrestricted => Some(Fam::Unrestricted),
                    ArgMult::Sum(xs) if xs.is_empty() => Some(Fam::Erased),
                    ArgMult::Max(xs) if xs.is_empty() => panic!("argmulti {:?} has empty Max", k),
                    _ => None,
                }
                .map(|j| (*k, j))
            })
            .collect();
        let mut finmlen = 0;

        if finm.is_empty() {
            return Ok(());
        }

        while finmlen != finm.len() {
            for (k, v) in &mut self.a {
                if finm.contains_key(k) {
                    continue;
                }
                if let Some(v2) = resolve_argmulti(v, &finm, *k) {
                    tracing::debug!("argmultis: resolved {} <- {:?}", k, v2);
                    finm.insert(*k, v2);
                }
            }
            finmlen = finm.len();
        }

        //let mut g_modified = false;
        let mut notify_cgs = BTreeSet::default();
        let mut to_unify = Vec::new();
        for (k, i) in &mut self.g {
            let ty_new = match i.kind.take() {
                Some(Tcgk::Arrow {
                    multi,
                    ty_arg,
                    ty_ret,
                }) => {
                    if let Some(&arg_multi) = finm.get(&multi) {
                        Ty::Arrow {
                            arg_multi,
                            arg: Box::new(ty_arg),
                            ret: Box::new(ty_ret),
                        }
                    } else {
                        i.kind = Some(Tcgk::Arrow {
                            multi,
                            ty_arg,
                            ty_ret,
                        });
                        continue;
                    }
                }
                x => {
                    i.kind = x;
                    continue;
                }
            };
            tracing::trace!("argmultis/g: resolved {} <- {:?}", k, ty_new);
            if let Some(x) = &i.ty {
                to_unify.push((x.clone(), ty_new));
            } else {
                i.ty = Some(ty_new);
            }
            //g_modified = true;
            notify_cgs.extend(i.listeners.iter().copied());
        }
        let notify_cgs = notify_cgs
            .into_iter()
            .flat_map(|i| self.m.get(&i))
            .copied()
            .collect();
        for (i, j) in to_unify {
            self.unify(&i, &j)?;
        }
        self.notify_cgs(notify_cgs)?;
        //if g_modified {
        //    self.self_resolve()?;
        //}
        Ok(())
    }
}
