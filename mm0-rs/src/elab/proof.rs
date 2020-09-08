//! The proof compacter, which takes an elaborated lisp proof s-expr and produces
//! a [`Proof`] object that will be stored in the environment.
//!
//! [`Proof`]: ../environment/struct.Proof.html

use std::rc::Rc;
use std::hash::Hash;
use std::ops::Index;
use std::result::Result as StdResult;
use std::mem;
use std::collections::{HashMap, hash_map::Entry};
use super::environment::{AtomID, Type};
use super::{LocalContext, ElabError, Result, Environment,
  SortID, TermID, ThmID, ExprNode, ProofNode, DeclKey};
use super::lisp::{LispVal, LispKind, Uncons, InferTarget, print::FormatEnv};
use super::local_context::{InferSort, try_get_span_from};
use crate::util::*;

/// This struct represents the context for the hash-consing step of proof compaction
#[derive(Debug)]
pub struct NodeHasher<'a> {
  /// The local context, which is used to resolve local hypotheses and subproofs.
  pub lc: &'a LocalContext,
  /// The formatting environment, used for error reporting and for access to the `Environment`.
  pub fe: FormatEnv<'a>,
  /// The initial variable map, which maps variable names to their indices.
  pub var_map: HashMap<AtomID, usize>,
  /// The file span for the theorem, used for error reporting.
  pub fsp: FileSpan,
}

impl<'a> NodeHasher<'a> {
  /// Construct a new `NodeHasher`, using the `LocalContext` to construct the
  /// variable map.
  pub fn new(lc: &'a LocalContext, fe: FormatEnv<'a>, fsp: FileSpan) -> Self {
    let mut var_map = HashMap::new();
    for (i, &(_, a, _)) in lc.var_order.iter().enumerate() {
      if let Some(a) = a {var_map.insert(a, i);}
    }
    NodeHasher {lc, fe, var_map, fsp}
  }

  /// Construct an error at the given expression's location.
  fn err(&self, e: &LispKind, msg: impl Into<BoxError>) -> ElabError {
    self.err_sp(e.fspan().as_ref(), msg)
  }

  /// Construct an error at the given location.
  fn err_sp(&self, fsp: Option<&FileSpan>, msg: impl Into<BoxError>) -> ElabError {
    ElabError::new_e(try_get_span_from(&self.fsp, fsp), msg)
  }
}

/// A "hashable" type. We use this to abstract the difference between
/// [`ExprHash`] and [`ProofHash`]. The definition of `NodeHash` is mutually recursive
/// with the [`Dedup`] struct. A `NodeHash` type represents a nonrecursive shadow
/// of a recursive type (namely [`ExprNode`] and [`ProofNode`], respectively),
/// where recursive occurrences are replaced with indices tracked by the `Dedup` type.
/// Effectively, `Dedup` is acting as an arena allocator where the pointers are
/// replaced by integers.
///
/// [`ExprHash`]: enum.ExprHash.html
/// [`ProofHash`]: enum.ProofHash.html
/// [`Dedup`]: struct.Dedup.html
/// [`ExprNode`]: ../environment/enum.ExprNode.html
/// [`ProofNode`]: ../environment/enum.ProofNode.html
pub trait NodeHash: Hash + Eq + Sized {
  /// The variant that constructs a variable from an index.
  const REF: fn(usize) -> Self;

  /// Given a lisp expression `r` representing an element of the type,
  /// parse it into a `NodeHash` object. If the object has already been constructed,
  /// it may also return an index to the element in the `Dedup`.
  fn from<'a>(nh: &NodeHasher<'a>, fsp: Option<&FileSpan>, r: &LispVal,
    de: &mut Dedup<Self>) -> Result<StdResult<Self, usize>>;

  /// Calculate the variable dependence of a `NodeHash` object, given a function
  /// `deps` that will provide the dependencies of elements. Bump `bv` if this object
  /// is a dummy variable.
  fn vars(&self, bv: &mut u64, deps: impl Fn(usize) -> u64) -> u64;
}

/// The main hash-consing state object. This tracks previously hash-consed elements
/// and uses the `Hash` implementation required by `NodeHash` to hash elements of
/// the hash type `H`. (Since these objects may be somewhat large, we store them
/// behind an `Rc` so that they can go in both the map and the vec.)
#[derive(Debug)]
pub struct Dedup<H: NodeHash> {
  /// The map from hash objects to their assigned indexes. These indexes are
  /// incorporated in later hash objects, so hashing is constant time but equality
  /// of the `NodeHash` objects still implies deep equality of the trees that
  /// they represent.
  map: HashMap<Rc<H>, usize>,
  /// In order to deduplicate lisp expressions which already have internal sharing
  /// without traversing the same terms many times, we store a pointer hash here
  /// from each expression to its index, if we have already hashed it. We store
  /// a reference to the object in the map to ensure that it is not deallocated.
  /// (There is no safety problem here, but if the object gets deallocated and
  /// another takes its place, we can get a false positive hit in the map.)
  prev: HashMap<*const LispKind, (LispVal, usize)>,
  /// The list of allocated objects. At each index, we store `(hash, shared, deps)` where
  /// `hash` is the hash object, `shared` is true if this object has more than one
  /// reference, and `deps` is the dependencies of this expression
  /// (calculated as a useful side effect of deduplication).
  pub vec: Vec<(Rc<H>, bool, u64)>,
  /// `2 ^ n` where `n` is the number of bound variables currently allocated.
  /// (Yes, this puts a limit of 64 simultaneous bound variables. In fact the limit is
  /// lower than that, [55](../local_context/constant.MAX_BOUND_VARS.html),
  /// due to the way BV sets are stored in the compiled `.mmb` format.)
  bv: u64,
}

impl<H: NodeHash> Dedup<H> {
  /// Create a new `Dedup`, given the list of arguments ([`Term::args`]) in the context.
  ///
  /// [`Term::args`]: ../environment/struct.Term.html#structfield.args
  pub fn new(args: &[(Option<AtomID>, Type)]) -> Dedup<H> {
    let mut bv = 1;
    let vec: Vec<_> = args.iter().enumerate()
      .map(|(i, (_, t))| (Rc::new(H::REF(i)), true, match t {
        Type::Bound(_) => { let v = bv; bv *= 2; v }
        &Type::Reg(_, deps) => deps,
      })).collect();
    Dedup {
      map: vec.iter().enumerate().map(|(i, r)| (r.0.clone(), i)).collect(),
      prev: HashMap::new(),
      vec,
      bv,
    }
  }

  /// Insert a new hash object `v`, originating from lisp object `p`,
  /// into the `Dedup`, returning the allocated index.
  pub fn add(&mut self, p: LispVal, v: H) -> usize {
    let n = self.add_direct(v);
    self.prev.insert(&*p, (p, n));
    n
  }

  /// Insert a new hash object `v`, originating from lisp object `p`,
  /// into the `Dedup`, returning the allocated index.
  pub fn dedup(&mut self, nh: &NodeHasher<'_>, e: &LispVal) -> Result<usize> {
    let r = e.unwrapped_arc();
    let p: *const _ = &*r;
    Ok(match self.prev.get(&p) {
      Some(&(_, n)) => self.reuse(n),
      None => {
        let n = match H::from(nh, e.fspan().as_ref(), &r, self)? {
          Ok(v) => self.add_direct(v),
          Err(n) => n,
        };
        self.prev.insert(p, (r, n)); n
      }
    })
  }

  /// Convert a `Dedup<H>` to `Dedup<T>` given an injective function `f: H -> T`.
  /// Here injectivity is with respect to the `Eq` implementations on `H` and `T`:
  /// If `f(x) == f(y)` then `x == y`.
  fn map_inj<T: NodeHash>(&self, mut f: impl FnMut(&H) -> T) -> Dedup<T> {
    let mut map = HashMap::new();
    let vec = self.vec.iter().enumerate().map(|(i, &(ref h, b, v))| {
      let t = Rc::new(f(h));
      map.insert(t.clone(), i);
      (t, b, v)
    }).collect();
    Dedup { map, prev: self.prev.clone(), vec, bv: self.bv }
  }
}

/// A trait that abstracts a few functions on `Dedup<H>`.
pub trait IDedup<H>: Index<usize, Output=Rc<H>> {
  /// Insert a new hash object `v` into the `Dedup`, returning the allocated index.
  /// Like [`add`], but does not add a record for the lisp data.
  ///
  /// [`add`]: struct.Dedup.html#method.add
  fn add_direct(&mut self, v: H) -> usize;

  /// Mark that an already allocated index `n` is being shared.
  fn reuse(&mut self, n: usize) -> usize;
}

impl<H: NodeHash> Index<usize> for Dedup<H> {
  type Output = Rc<H>;
  fn index(&self, n: usize) -> &Rc<H> { &self.vec[n].0 }
}

impl<H: NodeHash> IDedup<H> for Dedup<H> {
  fn add_direct(&mut self, v: H) -> usize {
    match self.map.entry(Rc::new(v)) {
      Entry::Vacant(e) => {
        let vec = &mut self.vec;
        let n = vec.len();
        let vars = e.key().vars(&mut self.bv, |i| vec[i].2);
        vec.push((e.key().clone(), false, vars));
        e.insert(n);
        n
      }
      Entry::Occupied(e) => {
        let &n = e.get();
        self.vec[n].1 = true;
        n
      }
    }
  }

  fn reuse(&mut self, n: usize) -> usize {
    self.vec[n].1 = true;
    n
  }
}

/// An iterator over the elements allocated by a `Dedup`, created by
/// the `IntoIterator` implementation for [`Dedup`].
///
/// [`Dedup`]: struct.Dedup.html
#[derive(Debug)]
pub struct DedupIter<'a, H: NodeHash>(std::slice::Iter<'a, (Rc<H>, bool, u64)>);

impl<'a, H: NodeHash> Iterator for DedupIter<'a, H> {
  type Item = (&'a H, bool);
  fn next(&mut self) -> Option<(&'a H, bool)> {
    self.0.next().map(|&(ref e, b, _)| (&**e, b))
  }
}

impl<'a, H: NodeHash> ExactSizeIterator for DedupIter<'a, H> {
  fn len(&self) -> usize { self.0.len() }
}

impl<'a, H: NodeHash> IntoIterator for &'a Dedup<H> {
  type Item = (&'a H, bool);
  type IntoIter = DedupIter<'a, H>;
  fn into_iter(self) -> DedupIter<'a, H> { DedupIter(self.vec.iter()) }
}


/// A "hash-consable" type. We use this to abstract the difference between
/// [`ExprNode`] and [`ProofNode`]. The `Hash` type here
/// ([`ExprHash`] and [`ProofHash`]) is a de-recursified
/// version of the type where all recursive occurrences are replaced by `usize`
/// indexes. This trait describes how hash objects can be reconstituted
/// into node objects.
///
/// This trait is mutually recursive with the [`Val`] type.
///
/// [`ExprHash`]: enum.ExprHash.html
/// [`ProofHash`]: enum.ProofHash.html
/// [`ExprNode`]: ../environment/enum.ExprNode.html
/// [`ProofNode`]: ../environment/enum.ProofNode.html
/// [`Val`]: enum.Val.html
pub trait Node: Sized {
  /// The type of hash objects.
  type Hash: NodeHash;
  /// The variant constuctor of this type for variables and backreferences.
  const REF: fn(usize) -> Self;
  /// Given a hash object, and a list of ids containing values that
  /// have previously been computed, reconstruct an element of the
  /// recursive type.
  fn from(e: &Self::Hash, ids: &mut [Val<Self>]) -> Self;
}

/// A constructed value corresponding to one index of a `Dedup`.
/// For unshared values, we use the `Built` constructor to store
/// a value of type `T` directly, while for shared values we only
/// store a reference to the `Ref` node index that was allocated to it.
/// The `Done` constructor represents an unshared value that has
/// already been "used up" by its referent.
#[derive(Debug)]
pub enum Val<T: Node> {
  /// An unshared value.
  Built(T),
  /// A shared value; the corresponding node is `T::REF(n)`.
  Ref(usize),
  /// An unshared value that has been moved away.
  Done}

impl<T: Node> Default for Val<T> {
  fn default() -> Self {Val::Done}
}

impl<T: Node> Val<T> {
  /// Take the value of type `T` out of this `Val`, leaving it
  /// in `Done` state for unshared values and "cloning" it
  /// for shared values.
  /// # Panics
  /// Calling `take` on an unshared value that has already been taken
  /// causes a panic. This is usually caused by a value being marked
  /// as unshared even though it appears twice in the proof.
  /// Calling [`reuse`] should ensure that this doesn't happen.
  ///
  /// [`reuse`]: trait.IDedup.html#tymethod.reuse
  pub fn take(&mut self) -> T {
    match mem::take(self) {
      Val::Built(x) => x,
      Val::Ref(n) => {*self = Val::Ref(n); T::REF(n)}
      Val::Done => panic!("taking a value twice")
    }
  }
}

/// Given a [`Dedup`] (or something that looks like one), consume it
/// and produce a pair `(ids, heap)` where `ids` is a set of
/// `Val<T>` nodes and `heap` is a list of shared values,
/// using the sharing annotations to determine whether to put the
/// values directly in `Built` nodes (for unshared nodes) or in
/// the `heap` with `Ref` nodes in the `ids`.
///
/// [`Dedup`]: struct.Dedup.html
pub fn build<'a, T: Node, D>(de: D) -> (Box<[Val<T>]>, Vec<T>)
where
  T::Hash: 'a,
  D: IntoIterator<Item=(&'a T::Hash, bool)>,
  D::IntoIter: ExactSizeIterator
{
  let it = de.into_iter();
  let mut ids: Vec<Val<T>> = Vec::with_capacity(it.len());
  let mut heap = Vec::new();
  for (e, b) in it {
    let node = T::from(e, &mut ids);
    if b {
      ids.push(Val::Ref(heap.len()));
      heap.push(node);
    } else {
      ids.push(Val::Built(node))
    }
  }
  (ids.into(), heap)
}

/// The `NodeHash` version of [`ExprNode`]. It has the same structure except that
/// all internal references to [`ExprNode`] are replaced by `usize` indexes.
///
/// [`ExprNode`]: ../environment/enum.ExprNode.html
#[derive(PartialEq, Eq, Hash, Debug)]
pub enum ExprHash {
  /// `Ref(n)` is a reference to heap element `n` (the first `args.len()` of them are the variables)
  Ref(usize),
  /// `Dummy(s, sort)` is a fresh dummy variable `s` with sort `sort`
  Dummy(AtomID, SortID),
  /// `App(t, nodes)` is an application of term constructor `t` to subterms
  App(TermID, Box<[usize]>),
}

impl NodeHash for ExprHash {
  const REF: fn(usize) -> Self = Self::Ref;

  fn from<'a>(nh: &NodeHasher<'a>, fsp: Option<&FileSpan>, r: &LispVal,
      de: &mut Dedup<Self>) -> Result<StdResult<Self, usize>> {
    Ok(Ok(match &**r {
      &LispKind::Atom(a) => match nh.var_map.get(&a) {
        Some(&i) => ExprHash::Ref(i),
        None => match nh.lc.vars.get(&a) {
          Some(&(true, InferSort::Bound(sort))) => ExprHash::Dummy(a, sort),
          _ => return Err(nh.err_sp(fsp, format!("variable '{}' not found", nh.fe.data[a].name))),
        }
      },
      LispKind::MVar(_, tgt) => return Err(nh.err_sp(fsp,
        format!("{}: {}", nh.fe.to(r), nh.fe.to(tgt)))),
      _ => {
        let mut u = Uncons::from(r.clone());
        let head = u.next().ok_or_else(||
          nh.err_sp(fsp, format!("bad expression {}", nh.fe.to(r))))?;
        let a = head.as_atom().ok_or_else(|| nh.err(&head, "expected an atom"))?;
        let tid = nh.fe.term(a).ok_or_else(||
          nh.err(&head, format!("term '{}' not declared", nh.fe.data[a].name)))?;
        let mut ns = Vec::new();
        for e in &mut u { ns.push(de.dedup(nh, &e)?) }
        if !u.exactly(0) {
          return Err(nh.err_sp(fsp, format!("bad expression {}", nh.fe.to(r))))
        }
        ExprHash::App(tid, ns.into())
      }
    }))
  }

  fn vars(&self, bv: &mut u64, deps: impl Fn(usize) -> u64) -> u64 {
    match self {
      &Self::Ref(n) => deps(n),
      &Self::Dummy(_, _) => (*bv, *bv *= 2).0,
      Self::App(_, es) => es.iter().fold(0, |a, &i| a | deps(i)),
    }
  }
}

impl Node for ExprNode {
  type Hash = ExprHash;
  const REF: fn(usize) -> Self = ExprNode::Ref;
  fn from(e: &Self::Hash, ids: &mut [Val<Self>]) -> Self {
    match *e {
      ExprHash::Ref(i) => ExprNode::Ref(i),
      ExprHash::Dummy(a, s) => ExprNode::Dummy(a, s),
      ExprHash::App(t, ref ns) => ExprNode::App(t,
        ns.iter().map(|&i| Val::take(&mut ids[i])).collect()),
    }
  }
}

impl Environment {
  /// Given a mapping of bound variables to lisp names,
  /// convert a packed representation of dependencies to a lisp list.
  pub fn deps(bvs: &[LispVal], xs: u64) -> LispVal {
    let mut deps = vec![];
    if xs != 0 {
      let mut bv = 1;
      for e in bvs {
        if xs & bv != 0 { deps.push(e.clone()) }
        bv *= 2;
      }
    }
    LispVal::list(deps)
  }

  /// Given a list of binders, convert them to a lisp list, updating
  /// the `heap` mapping of variable indexes to lisp names,
  /// and the `bvs` mapping of bound variable indexes to lisp names.
  pub fn binders(&self, bis: &[(Option<AtomID>, Type)],
      heap: &mut Vec<LispVal>, bvs: &mut Vec<LispVal>) -> LispVal {
    LispVal::list(bis.iter().map(|(a, t)| LispVal::list({
      let a = LispVal::atom(a.unwrap_or(AtomID::UNDER));
      heap.push(a.clone());
      match *t {
        Type::Bound(s) => {bvs.push(a.clone()); vec![a, LispVal::atom(self.sorts[s].atom)]}
        Type::Reg(s, xs) => vec![a, LispVal::atom(self.sorts[s].atom), Self::deps(&bvs, xs)]
      }
    })).collect::<Vec<_>>())
  }

  /// Convert an `ExprNode` object to a `LispVal`, under a context `heap`. If
  /// `ds` is set, it will accumulate any `Dummy` nodes that are encountered.
  pub fn expr_node(&self, heap: &[LispVal], ds: &mut Option<&mut Vec<LispVal>>, e: &ExprNode) -> LispVal {
    match *e {
      ExprNode::Ref(n) => heap[n].clone(),
      ExprNode::Dummy(a, s) => {
        let a = LispVal::atom(a);
        if let Some(ds) = ds {
          ds.push(LispVal::list(vec![a.clone(), LispVal::atom(self.sorts[s].atom)]));
        }
        a
      }
      ExprNode::App(t, ref es) => {
        let mut args = vec![LispVal::atom(self.terms[t].atom)];
        args.extend(es.iter().map(|e| self.expr_node(heap, ds, e)));
        LispVal::list(args)
      }
    }
  }
}

/// The `NodeHash` version of [`ProofNode`]. It has the same structure except that
/// all internal references to [`ProofNode`] are replaced by `usize` indexes.
///
/// [`ProofNode`]: ../environment/enum.ProofNode.html
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub enum ProofHash {
  /// `Ref(n)` is a reference to heap element `n` (the first `args.len()` of them are the variables).
  /// This could be an expr, proof, or conv depending on what is referenced.
  Ref(usize),
  /// `Dummy(s, sort)` is a fresh dummy variable `s` with sort `sort`
  Dummy(AtomID, SortID),
  /// `Term(term, args)` is an application of term constructor `term` to subterms
  Term(TermID, Box<[usize]>),
  /// `Hyp(i, e)` is hypothesis `i` (`hyps[i]` will be a reference to element),
  /// which is a proof of `|- e`.
  Hyp(usize, usize),
  /// `Thm(thm, args, res)` is a proof of `|- res` by applying theorem `thm` to arguments `args`.
  Thm(ThmID, Box<[usize]>, usize),
  /// `Conv(tgt, conv, proof)` is a proof of `|- tgt` if `proof: src` and `conv: tgt = src`.
  Conv(usize, usize, usize),
  /// `Refl(e): e = e`
  Refl(usize),
  /// `Refl(p): e2 = e1` if `p: e1 = e2`
  Sym(usize),
  /// `Cong(term, args): term a1 ... an = term b1 ... bn` if `args[i]: ai = bi`
  Cong(TermID, Box<[usize]>),
  /// `Unfold(term, args, lhs, sub_lhs, p)` is a proof of `lhs = rhs` if
  /// `lhs` is `term args` and `term` is a definition and `sub_lhs` is the result of
  /// substituting `args` into the definition of `term`, and `p: sub_lhs = rhs`
  Unfold(TermID, Box<[usize]>, usize, usize, usize),
}

impl ProofHash {
  /// Apply a substitution, while preserving sharing. The `nheap` array contains
  /// indexes for substituted subterms, in case we see the same subterm multiple times.
  pub fn subst(de: &mut impl IDedup<Self>,
    heap: &[ExprNode], nheap: &mut [Option<usize>], e: &ExprNode) -> usize {
    match *e {
      ExprNode::Ref(i) => match nheap[i] {
        Some(n) => de.reuse(n),
        None => {
          let n = Self::subst(de, heap, nheap, &heap[i]);
          nheap[i] = Some(n);
          n
        }
      },
      ExprNode::Dummy(_, _) => unreachable!(),
      ExprNode::App(t, ref es) => {
        let es2 = es.iter().map(|e| Self::subst(de, heap, nheap, e)).collect();
        de.add_direct(ProofHash::Term(t, es2))
      }
    }
  }

  /// Returns true if this proof term represents a conversion.
  pub fn is_conv(de: &impl IDedup<Self>, i: usize) -> bool {
    match *de[i] {
      ProofHash::Ref(j) => j < i && Self::is_conv(de, j),
      ProofHash::Dummy(_, _) |
      ProofHash::Term(_, _) |
      ProofHash::Hyp(_, _) |
      ProofHash::Thm(_, _, _) |
      ProofHash::Conv(_, _, _) => false,
      ProofHash::Refl(_) |
      ProofHash::Sym(_) |
      ProofHash::Cong(_, _) |
      ProofHash::Unfold(_, _, _, _, _) => true,
    }
  }

  /// Get the LHS (if `right = false`) or RHS (if `right = true`) of the conversion
  /// represented by proof term index `i`.
  pub fn conv_side(de: &mut impl IDedup<Self>, i: usize, right: bool) -> usize {
    match *de[i].clone() {
      ProofHash::Ref(j) => Self::conv_side(de, j, right),
      ProofHash::Dummy(_, _) |
      ProofHash::Term(_, _) |
      ProofHash::Hyp(_, _) |
      ProofHash::Thm(_, _, _) |
      ProofHash::Conv(_, _, _) => unreachable!(),
      ProofHash::Refl(e) => de.reuse(e),
      ProofHash::Sym(c) => Self::conv_side(de, c, !right),
      ProofHash::Cong(t, ref cs) => {
        let ns = cs.iter().map(|&c| Self::conv_side(de, c, right)).collect::<Vec<_>>();
        de.add_direct(ProofHash::Term(t, ns.into()))
      }
      ProofHash::Unfold(_, _, _, _, c) if right => Self::conv_side(de, c, true),
      ProofHash::Unfold(_, _, lhs, _, _) => de.reuse(lhs),
    }
  }

  /// If this is an expression, convert it to a conversion using `Refl`.
  /// For conversions, leave it as is.
  /// (This function should not be called on proof terms.)
  #[allow(clippy::wrong_self_convention)]
  pub fn as_conv(de: &mut impl IDedup<Self>, i: usize) -> usize {
    if Self::is_conv(de, i) {
      i
    } else {
      de.add_direct(ProofHash::Refl(i))
    }
  }
}

impl NodeHash for ProofHash {
  const REF: fn(usize) -> Self = Self::Ref;

  fn from<'a>(nh: &NodeHasher<'a>, fsp: Option<&FileSpan>, r: &LispVal,
      de: &mut Dedup<Self>) -> Result<StdResult<Self, usize>> {
    Ok(Ok(match &**r {
      &LispKind::Atom(a) => match nh.var_map.get(&a) {
        Some(&i) => ProofHash::Ref(i),
        None => match nh.lc.get_proof(a) {
          Some((_, _, p)) => return Ok(Err(de.dedup(nh, p)?)),
          None => match nh.lc.vars.get(&a) {
            Some(&(true, InferSort::Bound(sort))) => ProofHash::Dummy(a, sort),
            _ => return Err(nh.err_sp(fsp, format!("variable '{}' not found", nh.fe.data[a].name))),
          }
        }
      },
      LispKind::MVar(_, tgt) => return Err(nh.err_sp(fsp,
        format!("{}: {}", nh.fe.to(r), nh.fe.to(tgt)))),
      LispKind::Goal(tgt) => return Err(nh.err_sp(fsp, format!("|- {}", nh.fe.to(tgt)))),
      _ => {
        let mut u = Uncons::from(r.clone());
        let head = u.next().ok_or_else(||
          nh.err_sp(fsp, format!("bad expression {}", nh.fe.to(r))))?;
        let a = head.as_atom().ok_or_else(|| nh.err(&head, "expected an atom"))?;
        let adata = &nh.fe.data[a];
        match adata.decl {
          Some(DeclKey::Term(tid)) => {
            let mut ns = Vec::new();
            for e in u { ns.push(de.dedup(nh, &e)?) }
            if ns.iter().any(|&i| Self::is_conv(de, i)) {
              for i in &mut ns {*i = Self::as_conv(de, *i)}
              ProofHash::Cong(tid, ns.into())
            } else {
              ProofHash::Term(tid, ns.into())
            }
          }
          Some(DeclKey::Thm(tid)) => {
            let mut ns = Vec::new();
            for e in u { ns.push(de.dedup(nh, &e)?) }
            let td = &nh.fe.thms[tid];
            let mut heap = vec![None; td.heap.len()];
            let mut bvs: Vec<u64> = vec![];
            for (i, (_, t)) in td.args.iter().enumerate() {
              heap[i] = Some(ns[i]);
              let deps = de.vec[ns[i]].2;
              let ok = match t {
                Type::Bound(_) => {
                  bvs.push(deps);
                  ns[..i].iter().all(|&j| de.vec[j].2 & deps == 0)
                }
                &Type::Reg(_, mut d) => bvs.iter().all(|&bv| {
                  let old = d;
                  d /= 2;
                  old & 1 != 0 || bv & deps == 0
                }),
              };
              if !ok {
                let mut dvs = vec![];
                let mut bvs = vec![];
                for (i, (_, t)) in td.args.iter().enumerate() {
                  match t {
                    Type::Bound(_) => {
                      bvs.push(i);
                      dvs.extend((0..i).map(|j| (j, i)));
                    }
                    &Type::Reg(_, mut d) =>
                      dvs.extend(bvs.iter()
                        .filter(|_| { let old = d; d /= 2; old & 1 == 0 })
                        .map(|&j| (j, i)))
                  }
                }
                let mut err = format!("disjoint variable violation at {}", adata.name);
                let args: Vec<_> = Uncons::from(r.clone()).skip(1).collect();
                for (i, j) in dvs {
                  if de.vec[ns[i]].2 & de.vec[ns[j]].2 != 0 {
                    use std::fmt::Write;
                    write!(err, "\n  ({}, {}) -> ({}, {})",
                      nh.fe.to(&td.args[i].0.unwrap_or(AtomID::UNDER)),
                      nh.fe.to(&td.args[j].0.unwrap_or(AtomID::UNDER)),
                      nh.fe.pp(&args[i], 80), nh.fe.pp(&args[j], 80)).unwrap();
                  }
                }
                return Err(nh.err(&head, err))
              }
            }
            let rhs = Self::subst(de, &td.heap, &mut heap, &td.ret);
            ProofHash::Thm(tid, ns.into(), rhs)
          },
          None => match a {
            AtomID::CONV => match (u.next(), u.next(), u.next()) {
              (Some(tgt), Some(conv), Some(prf)) if u.exactly(0) => {
                let tgt = de.dedup(nh, &tgt)?;
                let conv = de.dedup(nh, &conv)?;
                let conv = Self::as_conv(de, conv);
                let prf = de.dedup(nh, &prf)?;
                ProofHash::Conv(tgt, Self::as_conv(de, conv), prf)
              }
              _ => return Err(nh.err_sp(fsp, format!("incorrect :conv format {}", nh.fe.to(r))))
            },
            AtomID::SYM => match u.next() {
              Some(p) if u.exactly(0) => {
                let p = de.dedup(nh, &p)?;
                ProofHash::Sym(Self::as_conv(de, p))
              }
              _ => return Err(nh.err_sp(fsp, format!("incorrect :sym format {}", nh.fe.to(r))))
            },
            AtomID::UNFOLD => {
              let (ty, es, prf) = match (u.next(), u.next(), u.next(), u.next()) {
                (Some(ty), Some(es), Some(prf), None) if u.exactly(0) => (ty, es, prf),
                (Some(ty), Some(es), Some(_), Some(prf)) if u.exactly(0) => (ty, es, prf),
                _ => return Err(nh.err_sp(fsp, format!("incorrect :unfold format {}", nh.fe.to(r))))
              };
              let tid = ty.as_atom().and_then(|a| nh.fe.term(a))
                .ok_or_else(|| nh.err(&ty, "expected a term"))?;
              let mut ns = Vec::new();
              for e in Uncons::from(es) { ns.push(de.dedup(nh, &e)?) }
              let lhs = de.add_direct(ProofHash::Term(tid, ns.clone().into()));
              let c = de.dedup(nh, &prf)?;
              let c = Self::as_conv(de, c);
              let l2 = Self::conv_side(de, c, false);
              ProofHash::Unfold(tid, ns.into(), lhs, l2, c)
            },
            _ => return Err(nh.err(&head, format!("term/theorem '{}' not declared", adata.name)))
          }
        }
      }
    }))
  }

  fn vars(&self, bv: &mut u64, deps: impl Fn(usize) -> u64) -> u64 {
    match self {
      &Self::Ref(n) => deps(n),
      &Self::Dummy(_, _) => (*bv, *bv *= 2).0,
      Self::Term(_, es) => es.iter().fold(0, |a, &i| a | deps(i)),
      _ => 0,
    }
  }
}

impl ExprHash {
  /// Convert an `ExprHash` directly to a `ProofHash`. This is an injective function,
  /// so it can be used with [`map_inj`].
  ///
  /// [`map_inj`]: struct.Dedup.html#method.map_inj
  pub fn to_proof(&self) -> ProofHash {
    match *self {
      ExprHash::Ref(i) => ProofHash::Ref(i),
      ExprHash::Dummy(a, s) => ProofHash::Dummy(a, s),
      ExprHash::App(t, ref ns) => ProofHash::Term(t, ns.clone()),
    }
  }
}

impl Dedup<ExprHash> {
  /// Efficiently maps a `Dedup<ExprHash>` to a `Dedup<ProofHash>`. This is
  /// useful for initializing the `Dedup<ProofHash>` based on a previous analysis
  /// of exprs used in the statement of the theorem.
  pub fn map_proof(&self) -> Dedup<ProofHash> {
    self.map_inj(ExprHash::to_proof)
  }
}

impl Node for ProofNode {
  type Hash = ProofHash;
  const REF: fn(usize) -> Self = ProofNode::Ref;
  fn from(e: &Self::Hash, ids: &mut [Val<Self>]) -> Self {
    match *e {
      ProofHash::Ref(i) => ProofNode::Ref(i),
      ProofHash::Dummy(a, s) => ProofNode::Dummy(a, s),
      ProofHash::Term(term, ref ns) => ProofNode::Term {
        term, args: ns.iter().map(|&i| Val::take(&mut ids[i])).collect()
      },
      ProofHash::Hyp(i, e) => ProofNode::Hyp(i, Box::new(Val::take(&mut ids[e]))),
      ProofHash::Thm(thm, ref ns, r) => ProofNode::Thm {
        thm, args: ns.iter().map(|&i| Val::take(&mut ids[i])).collect(),
        res: Box::new(Val::take(&mut ids[r]))
      },
      ProofHash::Conv(i, j, k) => ProofNode::Conv(Box::new((
        Val::take(&mut ids[i]), Val::take(&mut ids[j]), Val::take(&mut ids[k])))),
      ProofHash::Refl(i) => ProofNode::Refl(Box::new(Val::take(&mut ids[i]))),
      ProofHash::Sym(i) => ProofNode::Sym(Box::new(Val::take(&mut ids[i]))),
      ProofHash::Cong(term, ref ns) => ProofNode::Cong {
        term, args: ns.iter().map(|&i| Val::take(&mut ids[i])).collect()
      },
      ProofHash::Unfold(term, ref ns, l, m, c) => ProofNode::Unfold {
        term, args: ns.iter().map(|&i| Val::take(&mut ids[i])).collect(),
        res: Box::new((Val::take(&mut ids[l]), Val::take(&mut ids[m]), Val::take(&mut ids[c])))
      },
    }
  }
}

/// A structure for performing substitutions on expressions represented as lisp values.
#[derive(Debug)]
pub struct Subst<'a> {
  /// The ambient environment.
  env: &'a Environment,
  /// The heap (from the theorem statement).
  heap: &'a [ExprNode],
  /// The already computed substitutions for elements of the heap, with unknown
  /// values set to `#undef`.
  subst: Vec<LispVal>,
}

impl<'a> Subst<'a> {
  /// Contruct a new `Subst` object. `args` should be initialized to
  /// the arguments to the theorem application (possibly metavariables).
  pub fn new(env: &'a Environment, heap: &'a [ExprNode], mut args: Vec<LispVal>) -> Subst<'a> {
    args.resize(heap.len(), LispVal::undef());
    Subst {env, heap, subst: args}
  }

  /// Substitute in an `ExprNode`. This version does not support dummy variables,
  /// which means it can be used for theorem applications but not definition unfolding.
  pub fn subst(&mut self, e: &ExprNode) -> LispVal {
    match *e {
      ExprNode::Ref(i) => {
        let e = &self.subst[i];
        if e.is_def() {return e.clone()}
        let e = self.subst(&self.heap[i]);
        self.subst[i] = e.clone();
        e
      }
      ExprNode::Dummy(_, _) => unreachable!(),
      ExprNode::App(t, ref es) => {
        let mut args = vec![LispVal::atom(self.env.terms[t].atom)];
        args.extend(es.iter().map(|e| self.subst(e)));
        LispVal::list(args)
      }
    }
  }

  /// Substitute in an `ExprNode`. This version creates new metavariables
  /// when encountering `Dummy` nodes.
  pub fn subst_mut(&mut self, lc: &mut LocalContext, e: &ExprNode) -> LispVal {
    match *e {
      ExprNode::Ref(i) => {
        let e = &self.subst[i];
        if e.is_def() {return e.clone()}
        let e = self.subst_mut(lc, &self.heap[i]);
        self.subst[i] = e.clone();
        e
      }
      ExprNode::Dummy(_, s) => lc.new_mvar(InferTarget::Bound(self.env.sorts[s].atom), None),
      ExprNode::App(t, ref es) => {
        let mut args = vec![LispVal::atom(self.env.terms[t].atom)];
        args.extend(es.iter().map(|e| self.subst_mut(lc, e)));
        LispVal::list(args)
      }
    }
  }
}
