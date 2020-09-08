//! The local context, as well as the implementation of elaboration and
//! type inference for top level terms and declarations.

use std::ops::Deref;
use std::mem;
use std::result::Result as StdResult;
use std::collections::{HashMap, hash_map::Entry};
use itertools::Itertools;
use super::environment::{AtomID, Type as EType};
use crate::parser::ast::{Decl, Type, DepType, LocalKind};
use super::*;
use super::lisp::{LispVal, LispKind, Uncons, InferTarget, print::FormatEnv};
use super::proof::*;
use crate::util::*;

/// The infer status of a variable in a declaration. For example in
/// `def foo {x} (ph: wff x y): wff = $ all z ph $;`, `x` has no declared type
/// but is known to be bound, `y` is not declared at all but known to be a bound non-dummy,
/// and `z` is not declared and must be a bound dummy of type `var` (assuming
/// that `all` has type `var` for its first argument).
#[derive(Debug, DeepSizeOf)]
pub enum InferSort {
  /// This is a declared bound variable with the given sort.
  Bound(SortID),
  /// This is a declared regular variable with the given sort and dependencies.
  Reg(SortID, Box<[AtomID]>),
  /// This is a variable does not have a declared type.
  Unknown {
    /// The span of the first occurrence of the variable
    src: Span,
    /// True if this is known to be a bound variable, because it appears
    /// in a context where only bound variables are allowed.
    must_bound: bool,
    /// True if this is a dummy variable, because it was first identified
    /// in a definition body, rather than in part of a theorem statement.
    dummy: bool,
    /// The list of sorts that this variable should have. The keys of this map
    /// are sorts that this variable must coerce to, with `None` corresponding
    /// to an unknown provable sort; the values of the map are metavariables
    /// of the indicated sorts that are awaiting assignment when the dummy's
    /// actual sort is determined.
    sorts: Box<HashMap<Option<SortID>, LispVal>>,
  },
}

impl InferSort {
  fn new(src: Span) -> InferSort {
    InferSort::Unknown { src, must_bound: false, dummy: true, sorts: Box::new(HashMap::new()) }
  }
  /// The sort of this variable. For an unknown variable, it returns a sort iff
  /// this variable has inferred exactly one sort, not counting the "`None`" provable sort.
  pub fn sort(&self) -> Option<SortID> {
    match self {
      &InferSort::Bound(sort) => Some(sort),
      &InferSort::Reg(sort, _) => Some(sort),
      InferSort::Unknown {sorts, ..} => {
        let mut res = None;
        for s in sorts.keys() {
          if let Some(s) = *s {
            if mem::replace(&mut res, Some(s)).is_some() {return None}
          }
        }
        res
      }
    }
  }
}

/// The local context is the collection of proof-local data. This is manipulated
/// by lisp tactics in order to keep track of the proof state and eventually produce a proof.
#[derive(Default, Debug, DeepSizeOf)]
pub struct LocalContext {
  /// The collection of local variables. The key is the name of the variable, and the
  /// value is `(dummy, is)` where `dummy` is true if this is a dummy variable
  /// (internal to the definition or proof) and `is` is the inferred sort data of the variable.
  /// When multiple variables have the same name, only the last one will be in this map.
  pub vars: HashMap<AtomID, (bool, InferSort)>,
  /// The list of variables in order of declaration. This also stores the variable span,
  /// and the atom is none if this is an anonymous (`_`) variable.
  /// The `InferSort` contains the inferred type of the variable, but only for
  /// variables that are not in the `vars` hashmap because they are shadowed or anonymous.
  pub var_order: Vec<(Span, Option<AtomID>, Option<InferSort>)>,
  /// The list of active metavariables. `refine` will add metavariables to this list when
  /// creating them during elaboration, and it is periodically cleaned to remove assigned
  /// metavariables. The `usize` field of a `MVar` will refer to the variable's position in
  /// this list.
  pub mvars: Vec<LispVal>,
  /// The list of goals, or holes in the proof. This is the main user-facing state of a proof.
  /// This can be manipulated by user code, but the builtin tactics will manage this list
  /// automatically. When the set of goals is empty, the proof is complete.
  pub goals: Vec<LispVal>,
  /// The proof name map. The keys are subproof name bindings created by `have` or hypothesis
  /// names from the initial proof state, and the values are indexes into `proof_order`.
  pub proofs: HashMap<AtomID, usize>,
  /// The stored subproof data. The proofs are ordered for determinism but there is no real
  /// meaning associated to the order, except that later names shadow earlier names.
  /// The value is the name of the subproof, the type (theorem statement) of the proof,
  /// and the elaborated proof term.
  pub proof_order: Vec<(AtomID, LispVal, LispVal)>,
  /// The "closer", a user-configurable (using [`set-close-fn`]) callback that gets called
  /// at the end of a `focus` block.
  ///
  /// [`set-close-fn`]: ../lisp/enum.BuiltinProc.html#variant.SetCloseFn
  pub closer: LispVal,
}

fn new_mvar(mvars: &mut Vec<LispVal>, tgt: InferTarget, sp: Option<FileSpan>) -> LispVal {
  let n = mvars.len();
  let e = LispVal::new(LispKind::MVar(n, tgt));
  let e = LispVal::new_ref(if let Some(sp) = sp {e.span(sp)} else {e});
  mvars.push(e.clone());
  e
}

impl LocalContext {
  /// Create a new local context.
  pub fn new() -> LocalContext { Default::default() }

  /// Reset the local context. This is the same as assigning to `new()` except it
  /// is a bit more efficient because it reuses allocations.
  pub fn clear(&mut self) {
    self.vars.clear();
    self.var_order.clear();
    self.mvars.clear();
    self.goals.clear();
    self.proofs.clear();
    self.proof_order.clear();
    self.closer = LispVal::undef();
  }

  /// Set the list of goals to `gs`, after filtering the elements that are not
  /// goals or are already instantiated.
  pub fn set_goals(&mut self, gs: impl IntoIterator<Item=LispVal>) {
    self.goals.clear();
    for g in gs {
      if g.is_goal() {
        self.goals.push(if g.is_ref() {g} else {LispVal::new_ref(g)})
      }
    }
  }

  /// Create a new metavariable, and track it in the local context.
  pub fn new_mvar(&mut self, tgt: InferTarget, sp: Option<FileSpan>) -> LispVal {
    new_mvar(&mut self.mvars, tgt, sp)
  }

  fn var(&mut self, x: AtomID, sp: Span) -> &mut (bool, InferSort) {
    self.vars.entry(x).or_insert_with(|| (true, InferSort::new(sp)))
  }

  /// Add a variable occurrence (from a location other than regular variable
  /// declarations) to the inference context. We pass the span of the variable,
  /// the name (or `_`), and whether it is in a dummy context (i.e. the body of a `def`)
  /// and the expected sort.
  ///
  /// It returns true if the variable was already in the binder list.
  fn push_var(&mut self, sp: Span, a: Option<AtomID>, (dummy, is): (bool, InferSort)) -> bool {
    if let Some(a) = a {
      let res = match self.vars.entry(a) {
        Entry::Vacant(e) => {e.insert((dummy, is)); false}
        Entry::Occupied(mut e) => {e.insert((dummy, is)); true}
      };
      if !dummy {self.var_order.push((sp, Some(a), None))}
      res
    } else {
      if !dummy {self.var_order.push((sp, None, Some(is)))}
      false
    }
  }

  /// Remove all metavariables that are already assigned, and renumber those
  /// that remain, so that the printed names `?a`, `?b` etc. stay simple.
  pub fn clean_mvars(&mut self) {
    let mut i = 0;
    self.mvars.retain(|e| e.as_ref_(|e| {
      e.unwrapped_mut(|e| {
        if let LispKind::MVar(n, _) = e {*n = i; i += 1; true}
        else {false}
      }).unwrap_or_else(|| {
        match **e {
          LispKind::MVar(n, ty) => {
            if n != i {*e = LispKind::MVar(i, ty).decorate_span(&e.fspan())}
            i += 1; true
          }
          _ => false,
        }
      })
    }).unwrap())
  }

  /// Get a subproof by name.
  pub fn get_proof(&self, a: AtomID) -> Option<&(AtomID, LispVal, LispVal)> {
    self.proofs.get(&a).map(|&i| &self.proof_order[i])
  }

  /// Insert a new subproof.
  pub fn add_proof(&mut self, a: AtomID, e: LispVal, p: LispVal) {
    self.proofs.insert(a, self.proof_order.len());
    self.proof_order.push((a, e, p));
  }
}

#[repr(C)]
struct ElabTerm<'a> {
  lc: &'a LocalContext,
  fe: FormatEnv<'a>,
  fsp: FileSpan,
}

#[repr(C)]
struct ElabTermMut<'a> {
  lc: &'a mut LocalContext,
  src: &'a LinedString,
  env: &'a mut Environment,
  fsp: FileSpan,
  spans: &'a mut Spans<ObjectKind>,
}

impl<'a> Deref for ElabTermMut<'a> {
  type Target = ElabTerm<'a>;
  fn deref(&self) -> &ElabTerm<'a> {
    unsafe { &*(self as *const _ as *const _) }
  }
}

/// Get the span from the lisp value `e`, but only if it lies inside the
/// span `fsp`, otherwise return `fsp`. (This prevents errors in
/// one statement from causing error reports further up the file or
/// even in another file.)
pub fn try_get_span(fsp: &FileSpan, e: &LispKind) -> Span {
  try_get_span_from(fsp, e.fspan().as_ref())
}

/// Get the span from `fsp2`, but only if it lies inside the
/// span `fsp`, otherwise return `fsp`. (This prevents errors in
/// one statement from causing error reports further up the file or
/// even in another file.)
pub fn try_get_span_from(fsp: &FileSpan, fsp2: Option<&FileSpan>) -> Span {
  match fsp2 {
    Some(fsp2) if fsp.file == fsp2.file && fsp2.span.start >= fsp.span.start => fsp2.span,
    _ => fsp.span,
  }
}

impl Environment {
  /// Construct the proof term corresponding to a coercion `c`.
  pub fn apply_coe(&self, fsp: &Option<FileSpan>, c: &Coe, res: LispVal) -> LispVal {
    fn apply(c: &Coe, f: &mut impl FnMut(TermID, LispVal) -> LispVal, e: LispVal) -> LispVal {
      match c {
        &Coe::One(_, tid) => f(tid, e),
        Coe::Trans(c1, _, c2) => {let e2 = apply(c1, f, e); apply(c2, f, e2)}
      }
    }
    apply(c, &mut |tid, e| LispKind::List(
      vec![LispVal::atom(self.terms[tid].atom), e].into()).decorate_span(fsp), res)
  }
}

impl<'a> ElabTerm<'a> {
  fn new(elab: &'a Elaborator, sp: Span) -> Self {
    ElabTerm {fsp: elab.fspan(sp), fe: elab.format_env(), lc: &elab.lc}
  }

  fn try_get_span(&self, e: &LispKind) -> Span {
    try_get_span(&self.fsp, e)
  }

  fn err(&self, e: &LispKind, msg: impl Into<BoxError>) -> ElabError {
    ElabError::new_e(self.try_get_span(e), msg)
  }

  fn coerce(&self, src: &LispVal, from: SortID, res: LispKind, tgt: InferTarget) -> Result<LispVal> {
    let fsp = src.fspan();
    let res = res.decorate_span(&fsp);
    let to = match tgt {
      InferTarget::Unknown => return Ok(res),
      InferTarget::Provable if self.fe.sorts[from].mods.contains(Modifiers::PROVABLE) => return Ok(res),
      InferTarget::Provable => *self.fe.pe.coe_prov.get(&from).ok_or_else(||
        self.err(src, format!("type error: expected provable sort, got {}", self.fe.sorts[from].name)))?,
      InferTarget::Reg(to) => self.fe.data[to].sort.unwrap(),
      InferTarget::Bound(_) => return Err(
        self.err(src, format!("expected a variable, got {}", self.fe.to(src))))
    };
    if from == to {return Ok(res)}
    if let Some(c) = self.fe.pe.coes.get(&from).and_then(|m| m.get(&to)) {
      Ok(self.fe.apply_coe(&fsp, c, res))
    } else {
      Err(self.err(&src,
        format!("type error: expected {}, got {}", self.fe.sorts[to].name, self.fe.sorts[from].name)))
    }
  }

  fn other(&self, e: &LispVal, _: InferTarget) -> Result<LispVal> {
    Err(self.err(e, "Not a valid expression"))
  }

  fn infer_sort(&self, e: &LispKind) -> Result<SortID> {
    e.unwrapped(|r| match r {
      &LispKind::Atom(a) => match self.lc.vars.get(&a) {
        None => Err(self.err(e, "variable not found")),
        Some(&(_, InferSort::Bound(sort))) => Ok(sort),
        Some(&(_, InferSort::Reg(sort, _))) => Ok(sort),
        Some((_, InferSort::Unknown {..})) => panic!("finalized vars already"),
      },
      LispKind::List(es) if !es.is_empty() => {
        let a = es[0].as_atom().ok_or_else(|| self.err(&es[0], "expected an atom"))?;
        let tid = self.fe.term(a).ok_or_else(||
          self.err(&es[0], format!("term '{}' not declared", self.fe.data[a].name)))?;
        Ok(self.fe.terms[tid].ret.0)
      }
      _ => Err(self.err(e, "invalid expression"))
    })
  }
}

impl<'a> ElabTermMut<'a> {
  fn new(elab: &'a mut Elaborator, sp: Span) -> Self {
    ElabTermMut {
      fsp: elab.fspan(sp),
      src: &elab.ast.source,
      env: &mut elab.env,
      lc: &mut elab.lc,
      spans: &mut elab.spans,
    }
  }

  fn spans_insert(&mut self, e: &LispKind, k: impl FnOnce() -> ObjectKind) {
    if let Some(fsp) = e.fspan() {
      if self.fsp.file.ptr_eq(&fsp.file) {
        self.spans.insert_if(fsp.span, k)
      }
    }
  }

  fn atom(&mut self, e: &LispVal, a: AtomID, tgt: InferTarget) -> Result<LispVal> {
    macro_rules! fe {() => {FormatEnv {source: self.src, env: self.env}}}
    let a = if a == AtomID::UNDER {
      let mut n = 1;
      loop {
        let a = self.env.get_atom(&format!("_{}", n));
        if !self.lc.vars.contains_key(&a) {break a}
        n += 1;
      }
    } else {a};
    let is = &mut self.lc.vars.entry(a).or_insert_with({
      let fsp = &self.fsp;
      move || (true, InferSort::new(try_get_span(fsp, e)))
    }).1;
    let res = match (is, tgt) {
      (InferSort::Reg {..}, InferTarget::Bound(_)) =>
        Err(self.err(e, "expected a bound variable, got regular variable")),
      (&mut InferSort::Bound(sort), InferTarget::Bound(sa)) => {
        let s = self.fe.data[sa].sort.unwrap();
        if s == sort {Ok(LispKind::Atom(a).decorate_span(&e.fspan()))}
        else {
          Err(self.err(e,
            format!("type error: expected {}, got {}", fe!().sorts[s].name, self.fe.sorts[sort].name)))
        }
      }
      (InferSort::Unknown {src, must_bound, sorts, ..}, tgt) => {
        let s = match tgt {
          InferTarget::Bound(sa) => {*must_bound = true; Some(fe!().data[sa].sort.unwrap())}
          InferTarget::Reg(sa) => Some(fe!().data[sa].sort.unwrap()),
          _ => None,
        };
        let mvars = &mut self.lc.mvars;
        let sp = FileSpan {file: self.fsp.file.clone(), span: *src};
        Ok(sorts.entry(s).or_insert_with(|| new_mvar(mvars, tgt, Some(sp))).clone())
      }
      (&mut InferSort::Reg(sort, _), tgt) => self.coerce(e, sort, LispKind::Atom(a), tgt),
      (&mut InferSort::Bound(sort), tgt) => self.coerce(e, sort, LispKind::Atom(a), tgt),
    };
    self.spans_insert(e, || ObjectKind::Var(a));
    res
  }

  fn list(&mut self, e: &LispVal,
    mut it: impl Iterator<Item=LispVal>, tgt: InferTarget) -> Result<LispVal> {
    let t = it.next().unwrap();
    let a = t.as_atom().ok_or_else(|| self.err(&t, "expected an atom"))?;
    if self.lc.vars.contains_key(&a) {
      return Err(self.err(&t,
        format!("term '{}' is shadowed by a local variable", self.fe.data[a].name)))
    }
    let tid = self.fe.term(a).ok_or_else(||
      self.err(&t, format!("term '{}' not declared", self.fe.data[a].name)))?;
    let sp1 = self.try_get_span(e);
    self.spans_insert(&t, || ObjectKind::Term(tid, sp1));
    let tdata = &self.fe.env.terms[tid];
    let mut tys = tdata.args.iter();
    let mut args = vec![LispKind::Atom(a).decorate_span(&t.fspan())];
    while let Some(arg) = it.next() {
      let tgt = match tys.next() {
        None => return Err(ElabError::new_e(sp1,
          format!("expected {} arguments, got {}", tdata.args.len(), args.len() + it.count()))),
        Some(&(_, EType::Bound(s))) => InferTarget::Bound(self.fe.sorts[s].atom),
        Some(&(_, EType::Reg(s, _))) => InferTarget::Reg(self.fe.sorts[s].atom),
      };
      args.push(self.expr(&arg, tgt)?);
    }
    if tys.next().is_some() {
      return Err(ElabError::new_e(sp1,
        format!("expected {} arguments, got {}", tdata.args.len(), args.len() - 1)))
    }
    self.coerce(e, tdata.ret.0, LispKind::List(args.into()), tgt)
  }

  // TODO: Unify this with RState::RefineExpr
  fn expr(&mut self, e: &LispVal, tgt: InferTarget) -> Result<LispVal> {
    e.unwrapped(|r| match r {
      &LispKind::Atom(a) if self.lc.vars.contains_key(&a) => self.atom(e, a, tgt),
      &LispKind::Atom(a) if self.fe.term(a).is_some() =>
        self.list(e, Some(e.clone()).into_iter(), tgt),
      &LispKind::Atom(a) => self.atom(e, a, tgt),
      LispKind::DottedList(es, r) if es.is_empty() => self.expr(r, tgt),
      LispKind::List(es) if es.len() == 1 => self.expr(&es[0], tgt),
      LispKind::List(_) | LispKind::DottedList(_, _) if e.list_at_least(2) =>
        self.list(e, Uncons::from(e.clone()), tgt),
      _ => self.other(e, tgt),
    })
  }
}

#[derive(Default)]
struct BuildArgs {
  map: HashMap<AtomID, u64>,
  size: usize,
}

pub(crate) const MAX_BOUND_VARS: usize = 55;

impl BuildArgs {
  fn push_bound(&mut self, a: Option<AtomID>) -> Option<()> {
    if self.size >= MAX_BOUND_VARS {return None}
    if let Some(a) = a {self.map.insert(a, 1 << self.size);}
    self.size += 1;
    Some(())
  }

  fn deps(&self, v: &[AtomID]) -> u64 {
    let mut ret = 0;
    for &a in v { ret |= self.map[&a] }
    ret
  }

  fn push_var(&mut self, vars: &HashMap<AtomID, (bool, InferSort)>,
    a: Option<AtomID>, is: &Option<InferSort>) -> Option<EType> {
    match is.as_ref().unwrap_or_else(|| &vars[&a.unwrap()].1) {
      &InferSort::Bound(sort) => {
        self.push_bound(a)?;
        Some(EType::Bound(sort))
      },
      &InferSort::Reg(sort, ref deps) => {
        let n = self.deps(deps);
        if let Some(a) = a {self.map.insert(a, n);}
        Some(EType::Reg(sort, n))
      }
      InferSort::Unknown {..} => unreachable!(),
    }
  }
  fn push_dummies(&mut self, vars: &HashMap<AtomID, (bool, InferSort)>) -> Option<()> {
    for (&a, is) in vars {
      if let (true, InferSort::Bound {..}) = is {
        self.push_bound(Some(a))?
      }
    }
    Some(())
  }

  fn expr_deps(&self, env: &Environment, e: &LispKind) -> u64 {
    e.unwrapped(|r| match r {
      &LispKind::Atom(a) => self.map[&a],
      LispKind::List(es) if !es.is_empty() =>
        if let Some(tid) = es[0].as_atom().and_then(|a| env.term(a)) {
          let tdef = &env.terms[tid];
          let mut argbv = Vec::new();
          let mut out = 0;
          for ((_, ty), e) in tdef.args.iter().zip(&es[1..]) {
            let mut n = self.expr_deps(env, e);
            match ty {
              EType::Bound(_) => argbv.push(n),
              &EType::Reg(_, deps) => {
                let mut i = 1;
                for &arg in &argbv {
                  if deps & i != 0 { n &= !arg }
                  i *= 2;
                }
                out |= n;
              }
            }
          }
          let deps = tdef.ret.1;
          let mut i = 1;
          for arg in argbv {
            if deps & i != 0 { out |= arg }
            i *= 2;
          }
          out
        } else {unreachable!()},
      _ => unreachable!()
    })
  }
}

#[allow(variant_size_differences)]
enum InferBinder {
  Var(Option<AtomID>, (bool, InferSort)),
  Hyp(Option<AtomID>, LispVal),
}

impl Elaborator {
  /// Elaborate a binder's [`DepType`] with a given [`LocalKind`]. Enforces the requirements that (1)
  /// bound and dummy variables do not have dependencies, (2) regular variables do not depend
  /// on dummy variables. The bool in the result's pair indicates whether the variable is a dummy variable.
  ///
  /// [`DepType`]: ../parser/ast/struct.DepType.html
  /// [`LocalKind`]: ../parser/ast/enum.LocalKind.html
  fn elab_dep_type(&mut self, error: &mut bool, lk: LocalKind, d: &DepType) -> Result<(bool, InferSort)> {
    let a = self.env.get_atom(self.ast.span(d.sort));
    let sort = self.data[a].sort.ok_or_else(|| ElabError::new_e(d.sort, "sort not found"))?;
    self.spans.insert(d.sort, ObjectKind::Sort(sort));
    Ok(if lk.is_bound() {
      if !d.deps.is_empty() {
        self.report(ElabError::new_e(
          d.deps[0].start..d.deps.last().unwrap().end, "dependencies not allowed in curly binders or dummy variables"));
        *error = true;
      }
      (lk == LocalKind::Dummy, InferSort::Bound(sort))
    } else {
      let deps = d.deps.iter().map(|&sp| {
        let y = self.env.get_atom(self.ast.span(sp));
        self.spans.insert(sp, ObjectKind::Var(y));
        match self.lc.var(y, sp) {
          (_, InferSort::Unknown {dummy, must_bound, ..}) =>
            {*dummy = false; *must_bound = true}
          (true, InferSort::Bound {..}) => {
            self.report(ElabError::new_e(sp,
              "regular variables cannot depend on dummy variables"));
            *error = true;
          }
          _ => {}
        }
        y
      }).collect::<Vec<_>>().into();
      (false, InferSort::Reg(sort, deps))
    })
  }

  fn elab_binder(&mut self, error: &mut bool, sp: Option<Span>, lk: LocalKind, ty: Option<&Type>) -> Result<InferBinder> {
    let x = if lk == LocalKind::Anon {None} else {
      sp.map(|sp| {
        let a = self.env.get_atom(self.ast.span(sp));
        self.spans.insert(sp, ObjectKind::Var(a));
        a
      })
    };
    Ok(match ty {
      None => {
        let src = sp.unwrap();
        let fsp = self.fspan(src);
        if self.mm0_mode {
          self.report(ElabError::warn(src, "(MM0 mode) variable missing sort"))
        }
        let mv = self.lc.new_mvar(InferTarget::Unknown, Some(fsp));
        let dummy = lk == LocalKind::Dummy;
        InferBinder::Var(x, (dummy, InferSort::Unknown {
          src, must_bound: lk.is_bound(), dummy,
          sorts: Box::new(Some((None, mv)).into_iter().collect())
        }))
      },
      Some(Type::DepType(d)) => InferBinder::Var(x, self.elab_dep_type(error, lk, d)?),
      Some(&Type::Formula(f)) => {
        let e = self.parse_formula(f)?;
        let e = self.eval_qexpr(e)?;
        let e = self.elaborate_term(f.0, &e, InferTarget::Provable)?;
        InferBinder::Hyp(x, e)
      },
    })
  }

  fn elaborate_term(&mut self, sp: Span, e: &LispVal, tgt: InferTarget) -> Result<LispVal> {
    ElabTermMut::new(self, sp).expr(e, tgt)
  }

  fn infer_sort(&self, sp: Span, e: &LispKind) -> Result<SortID> {
    ElabTerm::new(self, sp).infer_sort(e)
  }

  fn finalize_vars(&mut self, dummy: bool) -> Vec<ElabError> {
    let mut errs = Vec::new();
    let mut newvars = Vec::new();
    for (&a, (new, is)) in &mut self.lc.vars {
      if let InferSort::Unknown {src, must_bound, dummy: d2, ref sorts} = *is {
        if self.mm0_mode {errs.push(ElabError::warn(src, "(MM0 mode) inferred variable type"))}
        match if sorts.len() == 1 {
          sorts.keys().next().unwrap().ok_or_else(|| ElabError::new_e(src, "could not infer type"))
        } else {
          let env = &self.env;
          sorts.keys().find_map(|s| s.filter(|&s| {
            match env.pe.coes.get(&s) {
              None => sorts.keys().all(|s2| s2.map_or(true, |s2| s == s2)),
              Some(m) => sorts.keys().all(|s2| s2.map_or(true, |s2| s == s2 || m.contains_key(&s2))),
            }
          })).ok_or_else(|| {
            ElabError::new_e(src, format!("could not infer consistent type from {{{}}}",
              sorts.keys().filter_map(|&k| k).map(|s| &env.sorts[s].name).format(", ")))
          })
        } {
          Ok(sort) => {
            for (s, e) in &**sorts {
              let mut val = LispVal::atom(a);
              if let Some(s) = *s {
                if s != sort {
                  let fsp = Some(FileSpan {file: self.path.clone(), span: src});
                  val = self.env.apply_coe(&fsp, &self.env.pe.coes[&sort][&s], val);
                }
              }
              if let LispKind::Ref(m) = &**e {
                *m.get_mut() = val;
              } else {unreachable!()}
            }
            let new2 = if (dummy && *new) || must_bound {
              *is = InferSort::Bound(sort);
              dummy && d2
            } else {
              *is = InferSort::Reg(sort, Box::new([]));
              false
            };
            if !new2 && *new {*new = false; newvars.push((src, a))}
          }
          Err(e) => errs.push(e),
        }
      }
    }
    newvars.sort_by_key(|&(_, a)| self.env.data[a].name.deref());
    let mut vec: Vec<_> = newvars.into_iter().map(|(src, a)| (src, Some(a), None)).collect();
    vec.append(&mut self.lc.var_order);
    self.lc.var_order = vec;
    errs
  }

  /// Elaborate a declaration (`term`, `axiom`, `def`, `theorem`).
  pub fn elab_decl(&mut self, full: Span, d: &Decl) -> Result<()> {
    let mut ehyps = Vec::new();
    let mut error = false;
    macro_rules! report {
      ($e:expr) => {{let e = $e; self.report(e); error = true;}};
      ($sp:expr, $e:expr) => {report!(ElabError::new_e($sp, $e))};
    }
    if self.mm0_mode && !d.mods.is_empty() {
      self.report(ElabError::warn(d.id, "(MM0 mode) decl modifiers not allowed"))
    }

    // log!("elab {}", self.ast.span(d.id));
    self.lc.clear();
    for bi in &d.bis {
      match self.elab_binder(&mut error, bi.local, bi.kind, bi.ty.as_ref()) {
        Err(e) => { self.report(e); error = true }
        Ok(InferBinder::Var(x, is)) => {
          if !ehyps.is_empty() {report!(bi.span, "hypothesis binders must come after variable binders")}
          if self.lc.push_var(bi.local.unwrap_or(bi.span), x, is) {
            report!(bi.local.unwrap(), "variable occurs twice in binder list");
          }
        }
        Ok(InferBinder::Hyp(x, e)) => ehyps.push((bi, x, e)),
      }
    }
    let atom = self.env.get_atom(self.ast.span(d.id));
    self.spans.set_decl(atom);
    match d.k {
      DeclKind::Term | DeclKind::Def => {
        for (bi, _, _) in ehyps {report!(bi.span, "term/def declarations have no hypotheses")}
        let ret = match &d.ty {
          None => {
            if self.mm0_mode {
              self.report(ElabError::warn(d.id, "(MM0 mode) return type required"))
            }
            None
          }
          Some(Type::Formula(f)) => return Err(ElabError::new_e(f.0, "sort expected")),
          Some(Type::DepType(ty)) => match self.elab_dep_type(&mut error, LocalKind::Anon, ty)?.1 {
            InferSort::Reg(sort, deps) => Some((ty.sort, sort, deps)),
            _ => unreachable!(),
          },
        };
        if d.k == DeclKind::Term {
          if let Some(v) = &d.val {report!(v.span, "term declarations have no definition")}
        } else if d.val.is_none() && !self.mm0_mode {
          self.report(ElabError::warn(d.id, "def declaration missing value"));
        }
        let val = match &d.val {
          None => None,
          Some(f) => (|| -> Result<Option<(Span, LispVal)>> {
            if self.mm0_mode {
              if let SExprKind::Formula(_) = f.k {} else {
                self.report(ElabError::warn(f.span, "(MM0 mode) expected formula"))
              }
            }
            let e = self.eval_lisp(f)?;
            Ok(Some((f.span, self.elaborate_term(f.span, &e, match ret {
              None => InferTarget::Unknown,
              Some((_, s, _)) => InferTarget::Reg(self.sorts[s].atom),
            })?)))
          })().unwrap_or_else(|e| {self.report(e); None})
        };
        for e in self.finalize_vars(true) {report!(e)}
        if error {return Ok(())}
        let mut args = Vec::with_capacity(self.lc.var_order.len());
        let mut ba = BuildArgs::default();
        for &(sp, a, ref is) in &self.lc.var_order {
          let ty = ba.push_var(&self.lc.vars, a, is).ok_or_else(||
            ElabError::new_e(sp, format!("too many bound variables (max {})", MAX_BOUND_VARS)))?;
          args.push((a, ty));
        }
        let (ret, val) = match val {
          None => match ret {
            None => return Err(ElabError::new_e(full, "expected type or value")),
            Some((_, s, ref deps)) => ((s, ba.deps(deps)),
              if d.k == DeclKind::Term {None} else {Some(None)})
          },
          Some((sp, val)) => {
            let s = self.infer_sort(sp, &val)?;
            if ba.push_dummies(&self.lc.vars).is_none() {
              return Err(ElabError::new_e(sp, format!("too many bound variables (max {})", MAX_BOUND_VARS)))
            }
            let deps = ba.expr_deps(&self.env, &val);
            let val = {
              let mut de = Dedup::new(&args);
              let nh = NodeHasher::new(&self.lc, self.format_env(), self.fspan(sp));
              let i = de.dedup(&nh, &val)?;
              let (mut ids, heap) = build(&de);
              Expr {heap, head: ids[i].take()}
            };
            match ret {
              None => ((s, deps), Some(Some(val))),
              Some((sp, s2, ref deps2)) => {
                if s != s2 {
                  return Err(ElabError::new_e(sp, format!("type error: expected {}, got {}",
                    self.env.sorts[s].name, self.env.sorts[s2].name)))
                }
                let n = ba.deps(deps2);
                if deps & !n != 0 {
                  return Err(ElabError::new_e(sp, format!("variables {{{}}} missing from dependencies",
                    ba.map.iter().filter_map(|(&a, &i)| {
                      if let InferSort::Bound {..} = self.lc.vars[&a].1 {
                        if i & deps & !n != 0 {Some(&self.data[a].name)} else {None}
                      } else {None}
                    }).format(", "))))
                }
                ((s2, n), Some(Some(val)))
              }
            }
          }
        };
        let t = Term {
          atom, args, ret, val,
          span: self.fspan(d.id),
          vis: d.mods,
          full,
        };
        let tid = self.env.add_term(atom, t.span.clone(), || t).map_err(|e| e.into_elab_error(d.id))?;
        self.spans.insert(d.id, ObjectKind::Term(tid, d.id));
      }
      DeclKind::Axiom | DeclKind::Thm => {
        if d.val.is_none() {
          for bi in &d.bis {
            if let LocalKind::Dummy = bi.kind {
              self.report(ElabError::warn(bi.local.unwrap_or(bi.span), "useless dummy variable"))
            }
          }
        }
        let eret = match &d.ty {
          None => return Err(ElabError::new_e(full, "return type required")),
          Some(Type::DepType(ty)) => return Err(ElabError::new_e(ty.sort, "expression expected")),
          &Some(Type::Formula(f)) => {
            let e = self.parse_formula(f)?;
            let e = self.eval_qexpr(e)?;
            self.elaborate_term(f.0, &e, InferTarget::Provable)?
          }
        };
        if d.k == DeclKind::Axiom {
          if let Some(v) = &d.val {report!(v.span, "axiom declarations have no definition")}
        } else if let Some(v) = &d.val {
          if self.mm0_mode {
            self.report(ElabError::warn(v.span, "(MM0 mode) theorems should not have proofs"))
          }
        } else if !self.mm0_mode {
          self.report(ElabError::warn(d.id, "theorem declaration missing value"))
        }
        for e in self.finalize_vars(false) {report!(e)}
        if error {return Ok(())}
        let mut args = Vec::with_capacity(self.lc.var_order.len());
        let mut ba = BuildArgs::default();
        for &(sp, a, ref is) in &self.lc.var_order {
          let ty = ba.push_var(&self.lc.vars, a, is).ok_or_else(||
            ElabError::new_e(sp, format!("too many bound variables (max {})", MAX_BOUND_VARS)))?;
          args.push((a, ty));
        }
        let mut de = Dedup::new(&args);
        let span = self.fspan(d.id);
        let nh = NodeHasher::new(&self.lc, self.format_env(), span.clone());
        let mut is = Vec::new();
        for &(bi, a, ref e) in &ehyps {
          if a.map_or(false, |a| self.lc.vars.contains_key(&a)) {
            return Err(ElabError::new_e(bi.span, "hypothesis shadows local variable"))
          }
          is.push((a, de.dedup(&nh, e)?))
        }
        let ir = de.dedup(&nh, &eret)?;
        let NodeHasher {var_map, fsp, ..} = nh;
        let (mut ids, heap) = build(&de);
        let hyps = is.iter().map(|&(a, i)| (a, ids[i].take())).collect();
        let ret = ids[ir].take();
        let proof = d.val.as_ref().map(|e| {
          if self.check_proofs {
            (|| -> Result<Option<Proof>> {
              let mut de: Dedup<ProofHash> = de.map_proof();
              let mut is2 = Vec::new();
              for (i, (_, a, e)) in ehyps.into_iter().enumerate() {
                if let Some(a) = a {
                  let p = LispVal::atom(a);
                  is2.push(de.add(p.clone(), ProofHash::Hyp(i, is[i].1)));
                  self.lc.add_proof(a, e, p)
                }
              }
              let g = LispVal::new_ref(LispVal::goal(self.fspan(e.span), eret));
              self.lc.goals = vec![g.clone()];
              self.elab_lisp(e)?;
              for g in mem::take(&mut self.lc.goals) {
                report!(try_get_span(&span, &g),
                  format!("|- {}", self.format_env().pp(&g.goal_type().unwrap(), 80)))
              }
              if error {return Ok(None)}
              let nh = NodeHasher {var_map, fsp, fe: self.format_env(), lc: &self.lc};
              let ip = de.dedup(&nh, &g)?;
              let (mut ids, heap) = build(&de);
              let hyps = is2.into_iter().map(|i| ids[i].take()).collect();
              let head = ids[ip].take();
              Ok(Some(Proof {heap, hyps, head}))
            })().unwrap_or_else(|e| {self.report(e); None})
          } else {None}
        });
        let t = Thm {
          atom, span, vis: d.mods, full,
          args, heap, hyps, ret, proof
        };
        let tid = self.env.add_thm(atom, t.span.clone(), || t).map_err(|e| e.into_elab_error(d.id))?;
        self.spans.insert(d.id, ObjectKind::Thm(tid));
      }
    }
    self.spans.lc = Some(mem::take(&mut self.lc));
    Ok(())
  }
}

/// This is a temporary structure returned by [`add_thm`] which implements the
/// `(add-thm! x bis hyps ret vis vtask)` user-level function, when `vtask` is a
/// lambda instead of a direct proof. In this case, we have to suspend adding
/// the theorem, store local variables in this structure, and execute the
/// user closure, calling [`finish_add_thm`] afterwards.
///
/// [`add_thm`]: ../struct.Elaborator.html#method.add_thm
/// [`finish_add_thm`]: ../struct.Elaborator.html#method.finish_add_thm
#[derive(Debug)]
pub struct AwaitingProof {
  thm: Thm,
  de: Dedup<ProofHash>,
  var_map: HashMap<AtomID, usize>,
  lc: Box<LocalContext>,
  is: Vec<usize>,
}

impl AwaitingProof {
  /// The theorem name being added.
  pub fn atom(&self) -> AtomID { self.thm.atom }

  /// Once the user closure completes, we can call this function to consume the suspended
  /// computation and finish adding the theorem.
  pub fn finish(self, elab: &mut Elaborator, fsp: FileSpan, proof: LispVal) -> Result<()> {
    let AwaitingProof {thm, de, var_map, lc, is} = self;
    elab.finish_add_thm(fsp, thm, Some(Some(ThmVal {de, var_map, lc: Some(lc), is, proof})))
  }
}

#[derive(Debug)]
struct ThmVal {
  de: Dedup<ProofHash>,
  var_map: HashMap<AtomID, usize>,
  lc: Option<Box<LocalContext>>,
  is: Vec<usize>,
  proof: LispVal
}

fn dummies(fe: FormatEnv<'_>, fsp: &FileSpan, lc: &mut LocalContext, e: &LispVal) -> Result<()> {
  macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or(fsp.clone()).span}}
  let mut dummy = |x: AtomID, es: &LispKind| -> Result<()> {
    let s = es.as_atom().ok_or_else(|| ElabError::new_e(sp!(es), "expected an atom"))?;
    let sort = fe.data[s].sort.ok_or_else(|| ElabError::new_e(sp!(es),
      format!("unknown sort '{}'", fe.to(&s))))?;
    if x != AtomID::UNDER {lc.vars.insert(x, (true, InferSort::Bound(sort)));}
    Ok(())
  };
  e.unwrapped(|r| {
    if let LispKind::AtomMap(m) = r {
      for (&a, e) in m {dummy(a, e)?}
    } else {
      for e in Uncons::from(e.clone()) {
        let mut u = Uncons::from(e.clone());
        if let (Some(ex), Some(es)) = (u.next(), u.next()) {
          let x = ex.as_atom().ok_or_else(|| ElabError::new_e(sp!(ex), "expected an atom"))?;
          dummy(x, &es)?;
        } else {
          return Err(ElabError::new_e(sp!(e), "invalid dummy arguments"))
        }
      }
    }
    Ok(())
  })
}

type Binder = (Option<AtomID>, EType);

impl Elaborator {
  fn deps(&self, fsp: &FileSpan, vars: &HashMap<AtomID, u64>, vs: LispVal) -> Result<(Box<[AtomID]>, u64)> {
    macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or(fsp.clone()).span}}
    let mut n = 0;
    let mut ids = Vec::new();
    for v in Uncons::from(vs) {
      let a = v.as_atom().ok_or_else(|| ElabError::new_e(sp!(v), "expected an atom"))?;
      n |= vars.get(&a).ok_or_else(|| ElabError::new_e(sp!(v),
        format!("undeclared variable '{}'", self.print(&v))))?;
      ids.push(a);
    }
    Ok((ids.into(), n))
  }

  fn binders(&self, fsp: &FileSpan, u: Uncons, (varmap, next_bv): &mut (HashMap<AtomID, u64>, u64)) ->
      Result<(LocalContext, Vec<Binder>)> {
    macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or(fsp.clone()).span}}
    let mut lc = LocalContext::new();
    let mut args = Vec::new();
    for e in u {
      let mut u = Uncons::from(e.clone());
      if let (Some(ea), Some(es)) = (u.next(), u.next()) {
        let a = ea.as_atom().ok_or_else(|| ElabError::new_e(sp!(ea), "expected an atom"))?;
        let a = if a == AtomID::UNDER {None} else {Some(a)};
        let s = es.as_atom().ok_or_else(|| ElabError::new_e(sp!(es), "expected an atom"))?;
        let sort = self.data[s].sort.ok_or_else(|| ElabError::new_e(sp!(es),
          format!("unknown sort '{}'", self.print(&s))))?;
        let (is, ty) = match u.next() {
          None => {
            if let Some(a) = a {
              if *next_bv >= 1 << MAX_BOUND_VARS {
                return Err(ElabError::new_e(fsp.span,
                  format!("too many bound variables (max {})", MAX_BOUND_VARS)))
              }
              varmap.insert(a, *next_bv);
              *next_bv *= 2;
            }
            (InferSort::Bound(sort), EType::Bound(sort))
          }
          Some(vs) => {
            let (deps, n) = self.deps(fsp, &varmap, vs)?;
            (InferSort::Reg(sort, deps), EType::Reg(sort, n))
          }
        };
        lc.push_var(sp!(ea), a, (false, is));
        args.push((a, ty))
      } else {
        return Err(ElabError::new_e(sp!(e),
          format!("binder syntax error: {}", self.print(&e))))
      }
    }
    Ok((lc, args))
  }

  fn visibility(&self, fsp: &FileSpan, e: &LispVal) -> Result<Modifiers> {
    macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or(fsp.clone()).span}}
    match e.as_atom() {
      None if e.exactly(0) => Ok(Modifiers::NONE),
      Some(AtomID::PUB) => Ok(Modifiers::PUB),
      Some(AtomID::ABSTRACT) => Ok(Modifiers::ABSTRACT),
      Some(AtomID::LOCAL) => Ok(Modifiers::LOCAL),
      _ => Err(ElabError::new_e(sp!(e), format!("expected visibility, got {}", self.print(e))))
    }
  }

  /// Parse and add a term/def declaration (this is called by the `(add-term!)` lisp function).
  pub fn add_term(&mut self, fsp: FileSpan, es: &[LispVal]) -> Result<()> {
    macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or_else(|| fsp.clone()).span}}
    let (x, bis, ret, val) = match es {
      [x, bis, ret] => (x, bis, ret, None),
      [x, bis, ret, vis, ds, val] => (x, bis, ret, Some((vis, ds, val))),
      _ => return Err(ElabError::new_e(fsp.span, "expected 3 or 6 arguments"))
    };
    let span = x.fspan().unwrap_or_else(|| fsp.clone());
    let x = x.as_atom().ok_or_else(|| ElabError::new_e(span.span, "expected an atom"))?;
    if self.data[x].decl.is_some() {
      return Err(ElabError::new_e(fsp.span,
        format!("duplicate term/def declaration '{}'", self.print(&x))))
    }
    let mut vars = (HashMap::new(), 1);
    let (mut lc, args) = self.binders(&fsp, Uncons::from(bis.clone()), &mut vars)?;
    let ret = match ret.as_atom() {
      Some(s) => {
        let s = self.data[s].sort.ok_or_else(|| ElabError::new_e(sp!(ret),
          format!("unknown sort '{}'", self.print(&s))))?;
        (s, 0)
      }
      None => {
        let mut u = Uncons::from(ret.clone());
        if let (Some(e), Some(vs)) = (u.next(), u.next()) {
          let s = e.as_atom().ok_or_else(|| ElabError::new_e(sp!(e), "expected an atom"))?;
          let s = self.data[s].sort.ok_or_else(|| ElabError::new_e(sp!(e),
            format!("unknown sort '{}'", self.print(&s))))?;
          (s, self.deps(&fsp, &vars.0, vs)?.1)
        } else {
          return Err(ElabError::new_e(sp!(ret), format!("syntax error: {}", self.print(ret))))
        }
      }
    };
    let (vis, val) = if let Some((evis, ds, val)) = val {
      let vis = self.visibility(&fsp, evis)?;
      if !vis.allowed_visibility(DeclKind::Def) {
        return Err(ElabError::new_e(sp!(evis), "invalid modifiers for this keyword"))
      }
      (vis, Some((|| -> Result<Option<Expr>> {
        dummies(self.format_env(), &fsp, &mut lc, ds)?;
        let mut de = Dedup::new(&args);
        let nh = NodeHasher::new(&lc, self.format_env(), fsp.clone());
        let i = de.dedup(&nh, val)?;
        let (mut ids, heap) = build(&de);
        Ok(Some(Expr {heap, head: ids[i].take()}))
      })().unwrap_or_else(|e| {
        self.report(ElabError::new_e(e.pos,
          format!("while adding {}: {}", self.print(&x), e.kind.msg())));
        None
      })))
    } else {(Modifiers::NONE, None)};
    let full = fsp.span;
    let t = Term {atom: x, span, full, vis, args, ret, val};
    self.env.add_term(x, fsp, || t).map_err(|e| e.into_elab_error(full))?;
    Ok(())
  }

  /// Parse and add a term/def declaration (this is called by the `(add-thm!)` lisp function).
  ///
  /// This function may either complete successfully, in which case it returns `Ok(Ok(()))`,
  /// or it may yield if the user provided proof term is a closure that requires evaluation,
  /// in which case it returns `Ok(Err((ap, proof_closure)))` where `(proof_closure)` should
  /// evaluate to some value `proof`, which can be passed to [`finish`] to finish adding
  /// the theorem to the environment.
  ///
  /// [`finish`]: local_context/struct.AwaitingProof.html#method.finish
  pub fn add_thm(&mut self, fsp: FileSpan, es: &[LispVal]) -> Result<StdResult<(), (AwaitingProof, LispVal)>> {
    macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or(fsp.clone()).span}}
    let (x, bis, hyps, ret, proof) = match es {
      [x, bis, hyps, ret] => (x, bis, hyps, ret, None),
      [x, bis, hyps, ret, vis, vtask] => (x, bis, hyps, ret, Some((vis, vtask.clone()))),
      _ => return Err(ElabError::new_e(fsp.span, "expected 4 or 6 arguments"))
    };
    let span = x.fspan().unwrap_or_else(|| fsp.clone());
    let x = x.as_atom().ok_or_else(|| ElabError::new_e(span.span, "expected an atom"))?;
    if self.data[x].decl.is_some() {
      return Err(ElabError::new_e(fsp.span,
        format!("duplicate axiom/theorem declaration '{}'", self.print(&x))))
    }
    let mut vars = (HashMap::new(), 1);
    let (mut lc, args) = self.binders(&fsp, Uncons::from(bis.clone()), &mut vars)?;
    // crate::server::log(format!("{}: {:#?}", self.print(&x), lc));
    let mut de = Dedup::new(&args);
    let nh = NodeHasher::new(&lc, self.format_env(), fsp.clone());
    // crate::server::log(format!("{}: {:#?}", self.print(&x), nh.var_map));
    let mut is = Vec::new();
    for e in Uncons::from(hyps.clone()) {
      let mut u = Uncons::from(e.clone());
      if let (Some(ex), Some(ty)) = (u.next(), u.next()) {
        let x = ex.as_atom().ok_or_else(|| ElabError::new_e(sp!(ex), "expected an atom"))?;
        let a = if x == AtomID::UNDER {None} else {Some(x)};
        is.push((a, de.dedup(&nh, &ty)?, ty));
      } else {
        return Err(ElabError::new_e(sp!(hyps), format!("syntax error: {}", self.print(hyps))))
      }
    }
    let ir = de.dedup(&nh, ret)?;
    let (mut ids, heap) = build(&de);
    let hyps = is.iter().map(|&(a, i, _)| {
      (a, ids[i].take())
    }).collect();
    let ret = ids[ir].take();
    let mut thm = Thm {
      atom: x, span, full: fsp.span,
      vis: Modifiers::NONE,
      proof: None,
      args, heap, hyps, ret };
    let res = if let Some((vis, proof)) = proof {
      thm.vis = self.visibility(&fsp, vis)?;
      if !thm.vis.allowed_visibility(DeclKind::Thm) {
        return Err(ElabError::new_e(sp!(vis), "invalid modifiers for this keyword"))
      }
      Some(if self.check_proofs {
        let mut de = de.map_proof();
        let var_map = nh.var_map;
        let is = is.into_iter().enumerate().filter_map(|(i, (a, j, ty))| {
          let a = a?;
          let p = LispVal::atom(a);
          lc.add_proof(a, ty, p.clone());
          Some(de.add(p, ProofHash::Hyp(i, j)))
        }).collect();
        let lc = Box::new(lc);
        if proof.is_proc() {
          return Ok(Err((AwaitingProof {thm, de, var_map, lc, is}, proof)))
        }
        Some(ThmVal {de, var_map, lc: Some(lc), is, proof})
      } else {None})
    } else {None};
    self.finish_add_thm(fsp, thm, res)?;
    Ok(Ok(()))
  }

  fn finish_add_thm(&mut self, fsp: FileSpan, mut t: Thm, res: Option<Option<ThmVal>>) -> Result<()> {
    macro_rules! sp {($e:expr) => {$e.fspan().unwrap_or(fsp.clone()).span}}
    t.proof = res.map(|res| res.and_then(|ThmVal {mut de, var_map, mut lc, is: is2, proof: e}| {
      (|| -> Result<Option<Proof>> {
        let mut u = Uncons::from(e.clone());
        let (ds, pf) = match (u.next(), u.next(), u.exactly(0)) {
          (Some(ds), Some(pf), true) => (ds, pf),
          _ => return Err(ElabError::new_e(sp!(e), "bad proof format, expected (ds proof)"))
        };
        let lc = lc.as_mut().map(Box::deref_mut).unwrap_or(&mut self.lc);
        let fe = FormatEnv {source: &self.ast.source, env: &self.env};
        dummies(fe, &fsp, lc, &ds)?;
        let nh = NodeHasher {var_map, lc, fe, fsp: fsp.clone()};
        let ip = de.dedup(&nh, &pf)?;
        let (mut ids, heap) = build(&de);
        let hyps = is2.into_iter().map(|i| ids[i].take()).collect();
        let head = ids[ip].take();
        Ok(Some(Proof {heap, hyps, head}))
      })().unwrap_or_else(|e| {
        self.report(ElabError::new_e(e.pos,
          format!("while adding {}: {}", self.print(&t.atom), e.kind.msg())));
        None
      })
    }));
    let sp = fsp.span;
    self.env.add_thm(t.atom, fsp, || t).map_err(|e| e.into_elab_error(sp))?;
    Ok(())
  }
}