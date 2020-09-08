module MM0.FromMM.Parser (parseMM) where

import Data.List
import Data.Char
import Data.Maybe
import Data.Default
import Control.Monad.Trans.State
import Control.Monad.Except
import qualified Text.ParserCombinators.ReadP as P
import qualified Data.ByteString.Lazy as B
import qualified Data.ByteString.Lazy.Char8 as C
import qualified Data.IntMap as I
import qualified Text.Read.Lex as L
import qualified Data.Map.Strict as M
import qualified Data.Sequence as Q
import qualified Data.Set as S
import qualified Data.Text as T
import qualified Data.Text.Encoding as T
import MM0.Kernel.Environment (SortData(..), SExpr(..))
import MM0.FromMM.Types
import MM0.Util

parseMM :: B.ByteString -> Either String MMDatabase
parseMM s = snd <$> runFromMMM (process (toks s))

toks :: B.ByteString -> [T.Text]
toks = filter (not . T.null) . map (T.decodeLatin1 . B.toStrict) .
  C.splitWith (`elem` [' ', '\n', '\t', '\r'])

data Sym = Const Const | Var Var deriving (Show)
type Fmla = [Sym]

data ParseTrie = PT {
  _ptConst :: Parser,
  _ptVar :: M.Map Sort ParseTrie,
  _ptDone :: Maybe (Sort, [MMExpr] -> MMExpr) }
type Parser = M.Map Const ParseTrie

type Scope = [([(Label, Hyp)], [[Label]], S.Set Label)]

data MMParserState = MMParserState {
  mParser :: Parser,
  mVMap :: M.Map Var (Sort, Label),
  mSyms :: M.Map T.Text Sym,
  mScope :: Scope,
  mDB :: MMDatabase }

instance Default MMParserState where
  def = MMParserState def def def [def] def

type FromMMM = StateT MMParserState (Either String)

modifyDB :: (MMDatabase -> MMDatabase) -> FromMMM ()
modifyDB f = modify $ \s -> s {mDB = f (mDB s)}

modifyMeta :: (MMMetaData -> MMMetaData) -> FromMMM ()
modifyMeta f = modifyDB $ \db -> db {mMeta = f (mMeta db)}

modifyND :: (MMNatDed -> MMNatDed) -> FromMMM ()
modifyND f = modifyMeta $ \m -> m {mND = f <$> mND m}

runFromMMM :: FromMMM a -> Either String (a, MMDatabase)
runFromMMM m = (\(a, t) -> (a, mDB t)) <$> runStateT m def

addConstant :: Const -> FromMMM ()
addConstant c = modify $ \s -> s {mSyms = M.insert c (Const c) (mSyms s)}

addVariable :: Var -> FromMMM ()
addVariable v = modify $ \s -> s {mSyms = M.insert v (Var v) (mSyms s)}

exprVars :: MMExpr -> S.Set Label
exprVars (SVar v) = S.singleton v
exprVars (App _ es) = foldMap exprVars es

fmlaVars :: Fmla -> S.Set Var
fmlaVars [] = S.empty
fmlaVars (Const _ : ss) = fmlaVars ss
fmlaVars (Var v : ss) = S.insert v (fmlaVars ss)

fmlaVHyps :: Fmla -> FromMMM (S.Set Label)
fmlaVHyps f = do t <- get; return (S.map (\v -> snd (mVMap t M.! v)) (fmlaVars f))

scopeAddHyp :: Label -> Hyp -> S.Set Var -> Scope -> Scope
scopeAddHyp _ _ _ [] = undefined
scopeAddHyp x h vs1 ((hs, ds, vs) : s) = ((x, h):hs, ds, S.union vs1 vs) : s

scopeAddDV :: [Label] -> Scope -> Scope
scopeAddDV _ [] = undefined
scopeAddDV ds' ((hs, ds, vs) : s) = (hs, ds' : ds, vs) : s

scopeOpen :: Scope -> Scope
scopeOpen [] = undefined
scopeOpen ss@((_, _, vs) : _) = ([], [], vs) : ss

scopeClose :: Scope -> Maybe Scope
scopeClose [] = undefined
scopeClose (_ : []) = Nothing
scopeClose (_ : s : ss) = Just (s : ss)

addHyp :: Label -> Hyp -> S.Set Label -> FromMMM ()
addHyp x h vs = do
  addStmt x (Hyp h)
  modify $ \t -> t {mScope = scopeAddHyp x h vs (mScope t)}

mkFrame :: Fmla -> FromMMM (Frame, M.Map Var Int)
mkFrame f = do
  t <- get
  let sc@((_, _, vs) : _) = mScope t
  vs' <- fmlaVHyps f
  return $ mapSnd fst $ build (mDB t) (S.union vs vs') sc ([], S.empty)

insertDVs :: (Label -> Bool) -> DVs -> [Label] -> DVs
insertDVs _ ds [] = ds
insertDVs f ds (v:vs) = let ds' = insertDVs f ds vs in
  if f v then foldl' (insertDV1 v) ds' vs else ds'
  where

  insertDV1 :: Label -> DVs -> Label -> DVs
  insertDV1 v1 ds' v2 | f v2 = S.insert (orientPair (v1, v2)) ds'
  insertDV1 _ ds' _ = ds'

build :: MMDatabase -> S.Set Label -> Scope -> Frame -> (Frame, (M.Map Var Int, Int))
build db vars = go where
  go [] fr = (fr, (M.empty, 0))
  go ((hs, ds, _) : sc) (hs', ds') =
    let (hs'', f) = insertHyps hs hs' in
    mapSnd f $ go sc (hs'', foldl' (insertDVs (`S.member` vars)) ds' ds)

  insertHyps :: [(Label, Hyp)] -> [(VarStatus, T.Text)] ->
    ([(VarStatus, T.Text)], (M.Map Var Int, Int) -> (M.Map Var Int, Int))
  insertHyps [] hs' = (hs', id)
  insertHyps ((x, EHyp _ _):hs) hs' = insertHyps hs ((VSHyp, x):hs')
  insertHyps ((x, VHyp s v):hs) hs' = (hs'', (\(m', n) -> (M.insert v n m', n+1)) . f) where
    hs2 = (if sPure (snd (mSorts db M.! s)) then VSBound else VSOpen, x) : hs'
    (hs'', f) = insertHyps hs (if S.member x vars then hs2 else hs')

addSort :: Sort -> Maybe Sort -> FromMMM ()
addSort x s2 = modifyDB $ \db -> db {
  mSorts = M.insert x (s2, SortData False False False False) (mSorts db),
  mDecls = mDecls db Q.|> Sort x }

addStmt :: Label -> Stmt -> FromMMM ()
addStmt x st = modifyDB $ \db -> db {
  mDecls = mDecls db Q.|> Stmt x,
  mStmts = M.insert x (Q.length (mDecls db), st) (mStmts db) }

process :: [T.Text] -> FromMMM ()
process [] = return ()
process ("$(" : "$j" : ss) = processJ (parseJ ss)
process ("$(" : ss) = readUntil "$)" ss >>= process . snd
process ("$c" : ss) = do
  (cs, ss') <- readUntil "$." ss
  mapM_ addConstant cs >> process ss'
process ("$v" : ss) = do
  (vs, ss') <- readUntil "$." ss
  mapM_ addVariable vs >> process ss'
process ("$d" : ss) = do
  (vs, ss') <- readUntil "$." ss
  t <- get
  let vs' = (\v -> snd (mVMap t M.! v)) <$> vs
  modify $ \t' -> t' {mScope = scopeAddDV vs' (mScope t')}
  process ss'
process ("${" : ss) = do
  modify $ \t -> t {mScope = scopeOpen (mScope t)}
  process ss
process ("$}" : ss) = do
  t <- get
  sc <- fromJustError "too many $}" (scopeClose (mScope t))
  put $ t {mScope = sc}
  process ss
process (x : "$f" : c : v : "$." : ss) = do
  modify $ \t -> t {mVMap = M.insert v (c, x) (mVMap t)}
  addHyp x (VHyp c v) S.empty
  process ss
process (x : "$e" : ss) = do
  (f, ss') <- withContext x $ readMath "$." ss
  (s, e) <- parseFmla f
  withContext x $ addHyp x (EHyp s e) (exprVars e)
  process ss'
process (x : "$a" : ss) = do
  (f, ss') <- withContext x $ readMath "$." ss
  withContext x $ do
    (fr, m) <- mkFrame f
    addThm x f fr m Nothing
  process ss'
process (x : "$p" : ss) = do
  (f, ss1) <- withContext x $ readMath "$=" ss
  (p, ss2) <- readProof ss1
  withContext x $ do
    (fr, m) <- mkFrame f
    t <- get
    pr <- lift $ trProof fr (mDB t) p
    addThm x f fr m (Just pr)
  process ss2
process (x : ss) = throwError ("wtf " ++ T.unpack x ++ show (take 100 ss))

addThm :: Label -> Fmla -> Frame -> M.Map Var Int ->
  Maybe ([(Label, Label)], MMProof) -> FromMMM ()
addThm x f@(Const s : _) fr m p = do
  db <- gets mDB
  case mSorts db M.!? s of
    Nothing -> throwError ("sort '" ++ T.unpack s ++ "' not declared")
    Just (Just s', _) -> do
      (_, e) <- parseFmla f
      addStmt x (Thm fr (s', e) p)
    Just (Nothing, _) -> do
      guardError "syntax axiom has $d" (null (snd fr))
      when (isNothing p) $
        let g = App x . reorderMap m f in
        modify $ \t -> t {
          mParser = ptInsert (mVMap t) g f (mParser t) }
      e <- parseFmla f
      addStmt x (Term fr e p)
addThm _ _ _ _ _ = error "malformed statement"

readUntil :: T.Text -> [T.Text] -> FromMMM ([T.Text], [T.Text])
readUntil u = go id where
  go _ [] = throwError "unclosed command"
  go f ("$(" : ss) = readUntil "$)" ss >>= go f . snd
  go f (s : ss) | s == u = return (f [], ss)
  go f (s : ss) = go (f . (s:)) ss

readMath :: T.Text -> [T.Text] -> FromMMM (Fmla, [T.Text])
readMath u ss = do
  (sy, ss') <- readUntil u ss
  t <- get
  f <- mapM (\s ->
    fromJustError ("unknown symbol '" ++ T.unpack s ++ "'") (mSyms t M.!? s)) sy
  return (f, ss')

readProof :: [T.Text] -> FromMMM ([T.Text], [T.Text])
readProof = go id where
  go _ [] = throwError "unclosed $p"
  go f ("$." : ss) = return (f [], ss)
  go f (s : ss) = go (f . (s:)) ss

data Numbers =
    NNum Int Numbers
  | NSave Numbers
  | NSorry Numbers
  | NEOF
  | NError

instance Show Numbers where
  showsPrec _ (NNum n ns) r = shows n (' ' : shows ns r)
  showsPrec _ (NSave ns) r = "Z " ++ shows ns r
  showsPrec _ (NSorry ns) r = "? " ++ shows ns r
  showsPrec _ NEOF r = r
  showsPrec _ NError r = "err" ++ r

numberize :: String -> Int -> Numbers
numberize "" n = if n == 0 then NEOF else NError
numberize ('?' : s) n = if n == 0 then NSorry (numberize s 0) else NError
numberize ('Z' : s) n = if n == 0 then NSave (numberize s 0) else NError
numberize (c : s) n | 'A' <= c && c <= 'Y' =
  if c < 'U' then
    let i = ord c - ord 'A' in
    NNum (20 * n + i) (numberize s 0)
  else
    let i = ord c - ord 'U' in
    numberize s (5 * n + i + 1)
numberize (_ : _) _ = NError

data HeapEl = HeapEl MMProof | HTerm Label Int | HThm Label Int deriving (Show)

trProof :: Frame -> MMDatabase -> [T.Text] -> Either String ([(Label, Label)], MMProof)
trProof (hs, _) db = \case
  "(" : p -> processPreloads p (mkHeap hs 0 Q.empty) 0 id
  _ -> throwError "normal proofs not supported"
  where

  mkHeap :: [(VarStatus, Label)] -> Int -> Q.Seq HeapEl -> Q.Seq HeapEl
  mkHeap [] _ heap = heap
  mkHeap ((s, h):hs') n heap = mkHeap hs' (n+1) (heap Q.|> HeapEl (PHyp s h n))

  processPreloads :: [T.Text] -> Q.Seq HeapEl -> Int ->
    ([(Label, Label)] -> [(Label, Label)]) -> Either String ([(Label, Label)], MMProof)
  processPreloads [] _ _ _ = throwError "unclosed parens in proof"
  processPreloads (")" : blocks) heap _ ds = do
    pt <- processBlocks (numberize (join (T.unpack <$> blocks)) 0) heap 0 []
    return (ds [], pt)
  processPreloads (st : p) heap sz ds = case getStmtM db st of
      Nothing -> throwError ("statement " ++ T.unpack st ++ " not found")
      Just (_, Hyp (VHyp s _)) ->
        processPreloads p (heap Q.|> HeapEl (PDummy st)) (sz + 1) (ds . ((st, s) :))
      Just (_, Hyp (EHyp _ _)) -> throwError "$e found in paren list"
      Just (_, Term (hs', _) _ _) ->
        processPreloads p (heap Q.|> HTerm st (length hs')) sz ds
      Just (_, Thm (hs', _) _ _) ->
        processPreloads p (heap Q.|> HThm st (length hs')) sz ds
      Just (_, Alias th) -> processPreloads (th : p) heap sz ds

  popn :: Int -> [MMProof] -> Either String ([MMProof], [MMProof])
  popn = go [] where
    go stk2 0 stk     = return (stk2, stk)
    go _    _ []      = throwError "stack underflow"
    go stk2 n (p:stk) = go (p:stk2) (n-1) stk

  processBlocks :: Numbers -> Q.Seq HeapEl -> Int -> [MMProof] -> Either String MMProof
  processBlocks NEOF _ _ [pt] = return pt
  processBlocks NEOF _ _ pts = throwError ("bad stack state " ++ show pts)
  processBlocks NError _ _ _ = throwError "proof block parse error"
  processBlocks (NSave _) _ _ [] = throwError "can't save empty stack"
  processBlocks (NSave p) heap sz (pt : pts) =
    processBlocks p (heap Q.|> HeapEl (PBackref sz)) (sz + 1) (PSave pt : pts)
  processBlocks (NSorry p) heap sz pts =
    processBlocks p heap sz (PSorry : pts)
  processBlocks (NNum i p) heap sz pts =
    case heap Q.!? i of
      Nothing -> throwError "proof backref index out of range"
      Just (HeapEl pt) -> processBlocks p heap sz (pt : pts)
      Just (HTerm x n) -> do
        (es, pts') <- withContext (T.pack $ show p) (popn n pts)
        processBlocks p heap sz (PTerm x es : pts')
      Just (HThm x n) -> do
        (es, pts') <- withContext (T.pack $ show p) (popn n pts)
        processBlocks p heap sz (PThm x es : pts')

data JComment =
    JKeyword T.Text JComment
  | JString T.Text JComment
  | JSemi JComment
  | JRest [T.Text]
  | JError String

parseJ :: [T.Text] -> JComment
parseJ [] = JError "unclosed $j comment"
parseJ ("$)" : ss) = JRest ss
parseJ (t : ss) = case T.unpack t of
  '\'' : s -> parseJString s id ss where
    parseJString [] _ [] = JError "unclosed $j string"
    parseJString [] f (s' : ss') = parseJString (T.unpack s') (f . (' ' :)) ss'
    parseJString ('\\' : 'n' : s') f ss' = parseJString s' (f . ('\n' :)) ss'
    parseJString ('\\' : '\\' : s') f ss' = parseJString s' (f . ('\\' :)) ss'
    parseJString ('\\' : '\'' : s') f ss' = parseJString s' (f . ('\'' :)) ss'
    parseJString ('\\' : s') _ _ = JError ("bad escape sequence '\\" ++ s' ++ "'")
    parseJString ('\'' : s') f ss' = JString (T.pack (f [])) (parseJ (T.pack s' : ss'))
    parseJString (c : s') f ss' = parseJString s' (f . (c :)) ss'
  s -> case P.readP_to_S L.lex s of -- TODO: better parser
    [(L.EOF, _)] -> parseJ ss
    [(L.Ident x, s')] -> JKeyword (T.pack x) (parseJ (T.pack s' : ss))
    [(L.Punc ";", s')] -> JSemi (parseJ (T.pack s' : ss))
    _ -> JError ("parse failed \"" ++ s ++ "\"" ++ T.unpack (head ss))

getManyJ :: String -> JComment -> ([T.Text] -> FromMMM ()) -> FromMMM ()
getManyJ name j f = go j id where
  go (JString s j') g = go j' (g . (s :))
  go (JSemi j') g = f (g []) >> processJ j'
  go _ _ = throwError ("bad $j '" ++ name ++ "' command")

processManyJ :: T.Text -> JComment -> (T.Text -> FromMMM ()) -> FromMMM ()
processManyJ name j f = go j where
  go (JString s j') = f s >> go j'
  go (JSemi j') = processJ j'
  go _ = throwError ("bad $j '" ++ T.unpack name ++ "' command")

processJ :: JComment -> FromMMM ()
processJ (JKeyword "syntax" j) = case j of
  JString s (JSemi j') -> addSort s Nothing >> processJ j'
  JString s1 (JKeyword "as" (JString s2 (JSemi j'))) -> do
    addSort s1 (Just s2)
    modifyDB $ \m -> m {mSorts =
      M.adjust (\(s, sd) -> (s, sd {sProvable = True})) s2 (mSorts m)}
    processJ j'
  _ -> throwError "bad $j 'syntax' command"
processJ (JKeyword "bound" j) = case j of
  JString s (JSemi j') -> do
    modifyDB $ \m -> m {mSorts =
      M.adjust (\(s', sd) -> (s', sd {sPure = True})) s (mSorts m)}
    processJ j'
  _ -> throwError "bad $j 'bound' command"
processJ (JKeyword "primitive" j) = processManyJ "primitive" j $ \s ->
  modifyMeta $ \m -> m {mPrim = S.insert s (mPrim m)}
processJ (JKeyword "justification" j) = case j of
  JString x (JKeyword "for" (JString df (JSemi j'))) -> do
    modifyMeta $ \m -> m {mJustification = M.insert df x (mJustification m)}
    processJ j'
  _ -> throwError "bad $j 'justification' command"
processJ (JKeyword "equality" j) = case j of
  JString x (JKeyword "from"
    (JString refl (JString sym (JString trans (JSemi j'))))) -> do
      db <- gets mDB
      s <- fromJustError ("equality '" ++ T.unpack x ++ "' has the wrong shape") (do
        (_, Term ([(_, v), _], _) _ _) <- getStmtM db x
        (_, Hyp (VHyp s _)) <- getStmtM db v
        return s)
      modifyMeta $ \m -> m { mEqual =
        let (m1, m2) = mEqual m in
        (M.insert s (Equality x refl sym trans) m1, M.insert x s m2) }
      processJ j'
  _ -> throwError "bad $j 'equality' command"
processJ (JKeyword "congruence" j) =
  processManyJ "congruence" j $ \x -> do
    db <- gets mDB
    t <- fromJustError ("congruence '" ++ T.unpack x ++ "' has the wrong shape") (do
      (_, Thm _ (_, App _ [App t _, _]) _) <- getStmtM db x
      return t)
    modifyMeta $ \m -> m {mCongr = M.insert t x (mCongr m)}
processJ (JKeyword "condequality" j) = case j of
  JString x (JKeyword "from" (JString th (JSemi j'))) -> do
    db <- gets mDB
    s <- fromJustError ("conditional equality '" ++ T.unpack x ++ "' has the wrong shape") (do
      (_, Term ([(_, v), _, _], _) _ _) <- getStmtM db x
      (_, Hyp (VHyp s _)) <- getStmtM db v
      return s)
    modifyMeta $ \m -> m {mCondEq = M.insert s (x, th) (mCondEq m)}
    processJ j'
  _ -> throwError "bad $j 'condequality' command"
processJ (JKeyword "condcongruence" j) =
  processManyJ "condcongruence" j $ \th ->
    modifyMeta $ \m -> m {mCCongr = th : mCCongr m}
processJ (JKeyword "notfree" j) = case j of
  JString x (JKeyword "from" (JString th (JSemi j'))) -> do
    db <- gets mDB
    s <- fromJustError ("not-free term '" ++ T.unpack x ++ "' has the wrong shape") (do
      (_, Term ([(_, v), (_, a)], _) _ _) <- getStmtM db x
      (_, Hyp (VHyp s1 _)) <- getStmtM db v
      (_, Hyp (VHyp s2 _)) <- getStmtM db a
      return (s1, s2))
    modifyMeta $ \m -> m {mNF = M.insert s (NF x th) (mNF m)}
    processJ j'
  _ -> throwError "bad $j 'notfree' command"
processJ (JKeyword "natded_init" j) = case j of
  JString p (JString c (JString e (JSemi j'))) -> do
    modifyMeta $ \m -> m {mND = Just (mkNatDed p c e)}
    processJ j'
  _ -> throwError "bad $j 'natded_init' command"
processJ (JKeyword "natded_assume" j) =
  getManyJ "natded_assume" j $ \xs -> do
    db <- gets mDB
    forM_ xs $ \y ->
      fromJustError ("natded_assume stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
    modifyND $ \nd -> nd {ndAssume = ndAssume nd ++ xs}
processJ (JKeyword "natded_weak" j) =
  getManyJ "natded_weak" j $ \xs -> do
    db <- gets mDB
    forM_ xs $ \y ->
      fromJustError ("natded_weak stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
    modifyND $ \nd -> nd {ndWeak = ndWeak nd ++ xs}
processJ (JKeyword "natded_cut" j) =
  getManyJ "natded_cut" j $ \xs -> do
    db <- gets mDB
    forM_ xs $ \y ->
      fromJustError ("natded_cut stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
    modifyND $ \nd -> nd {ndCut = ndCut nd ++ xs}
processJ (JKeyword "natded_true" j) = case j of
  JString x (JKeyword "with" j') ->
    getManyJ "natded_true" j' $ \xs -> do
      db <- gets mDB
      forM_ (x:xs) $ \y ->
        fromJustError ("natded_true stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
      modifyND $ \nd -> nd {ndTrue = Just $
        maybe (x, xs) (\(x', xs') -> (x', xs' ++ xs)) (ndTrue nd)}
  _ -> throwError "bad $j 'natded_true' command"
processJ (JKeyword "natded_imp" j) = case j of
  JString x (JKeyword "with" j') ->
    getManyJ "natded_imp" j' $ \xs -> do
      db <- gets mDB
      forM_ (x:xs) $ \y ->
        fromJustError ("natded_imp stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
      modifyND $ \nd -> nd {ndImp = Just $
        maybe (x, xs) (\(x', xs') -> (x', xs' ++ xs)) (ndImp nd)}
  _ -> throwError "bad $j 'natded_imp' command"
processJ (JKeyword "natded_and" j) = case j of
  JString x (JKeyword "with" j') ->
    getManyJ "natded_and" j' $ \xs -> do
      db <- gets mDB
      forM_ (x:xs) $ \y ->
        fromJustError ("natded_and stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
      modifyND $ \nd -> nd {ndAnd = Just $
        maybe (x, xs) (\(x', xs') -> (x', xs' ++ xs)) (ndAnd nd)}
  _ -> throwError "bad $j 'natded_and' command"
processJ (JKeyword "natded_or" j) = case j of
  JString x (JKeyword "with" j') ->
    getManyJ "natded_or" j' $ \xs -> do
      db <- gets mDB
      forM_ (x:xs) $ \y ->
        fromJustError ("natded_or stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
      modifyND $ \nd -> nd {ndOr = Just $
        maybe (x, xs) (\(x', xs') -> (x', xs' ++ xs)) (ndOr nd)}
  _ -> throwError "bad $j 'natded_or' command"
processJ (JKeyword "natded_not" j) = case j of
  JString not' (JString fal (JKeyword "with" j')) ->
    getManyJ "natded_not" j' $ \xs -> do
      db <- gets mDB
      forM_ (not':fal:xs) $ \y ->
        fromJustError ("natded_not stmt '" ++ T.unpack y ++ "' not found") (getStmtM db y)
      modifyND $ \nd -> nd {ndNot = Just $
        maybe (not', fal, xs) (\(x', f', xs') -> (x', f', xs' ++ xs)) (ndNot nd)}
  _ -> throwError "bad $j 'natded_not' command"
processJ (JKeyword "free_var" j) = case j of
  JString x (JKeyword "with" j') ->
    getManyJ "free_var" j' $ updateDecl x . S.fromList
  _ -> throwError "bad $j 'free_var' command"
  where
  updateDecl :: Label -> S.Set Var -> FromMMM ()
  updateDecl x ss = modifyDB $ \db ->
    let go (n, Term (hs, dv) e p) = (n, Term (updateHyp db ss <$> hs, dv) e p)
        go (n, Thm (hs, dv) e p) = (n, Thm (updateHyp db ss <$> hs, dv) e p)
        go _ = error "bad $j 'free_var' command"
    in db {mStmts = M.adjust go x $ mStmts db}

  updateHyp :: MMDatabase -> S.Set Var -> (VarStatus, Label) -> (VarStatus, Label)
  updateHyp db s (VSBound, l) = case getStmt db l of
    (_, Hyp (VHyp _ v)) | S.member v s -> (VSFree, l)
    _ -> (VSBound, l)
  updateHyp _ _ p = p
processJ (JKeyword "free_var_in" j) = case j of
  JString x (JKeyword "with" j') ->
    getManyJ "free_var" j' $ \vs -> do
      t <- get
      updateDecl x ((\v -> snd (mVMap t M.! v)) <$> vs)
  _ -> throwError "bad $j 'free_var_in' command"
  where
  updateDecl :: Label -> [Label] -> FromMMM ()
  updateDecl x vs = modifyDB $ \db -> db {mStmts = M.adjust go x $ mStmts db} where
    go (n, Term (hs, dv) e p) = (n, Term (hs, insertDVs (const True) dv vs) e p)
    go _ = error "bad $j 'free_var_in' command"
processJ (JKeyword "restatement" j) = case j of
  JString ax (JKeyword "of" (JString th (JSemi j'))) -> do
    db <- gets mDB
    (n, sax) <- case getStmtM db ax of
      Nothing -> throwError ("axiom '" ++ T.unpack ax ++ "' not found")
      Just (n, Thm fr e Nothing) -> return (n, (fr, e))
      _ -> throwError ("'" ++ T.unpack ax ++ "' is not an axiom")
    case getStmtM db th of
      Nothing -> throwError ("theorem '" ++ T.unpack ax ++ "' not found")
      Just (_, Thm fr e _) ->
        guardError ("restatement '" ++ T.unpack ax ++ "' does not match '" ++ T.unpack th ++ "'") $
          sax == (fr, e)
      _ -> throwError ("'" ++ T.unpack th ++ "' is not an axiom/theorem")
    modifyDB $ \db' -> db' {mStmts = M.insert ax (n, Alias th) $ mStmts db'}
    processJ j'
  _ -> throwError "bad $j 'restatement' command"

processJ (JKeyword _ j) = skipJ j
processJ (JRest ss) = process ss
processJ (JError e) = throwError e
processJ _ = throwError "bad $j comment"

skipJ :: JComment -> FromMMM ()
skipJ (JSemi j) = processJ j
skipJ (JKeyword _ j) = skipJ j
skipJ (JString _ j) = skipJ j
skipJ (JRest _) = throwError "unfinished $j statement"
skipJ (JError e) = throwError e

ptEmpty :: ParseTrie
ptEmpty = PT M.empty M.empty Nothing

ptInsert :: M.Map Var (Sort, Label) -> ([MMExpr] -> MMExpr) -> Fmla -> Parser -> Parser
ptInsert vm x (Const tc : fmla) = insert1 tc fmla where
  insert1 :: T.Text -> [Sym] -> Parser -> Parser
  insert1 i f = M.alter (Just . insertPT f . fromMaybe ptEmpty) i
  insertPT :: [Sym] -> ParseTrie -> ParseTrie
  insertPT [] (PT cs vs Nothing) = PT cs vs (Just (tc, x))
  insertPT [] (PT _ _ _) = error "duplicate parser"
  insertPT (Const c : f) (PT cs vs d) = PT (insert1 c f cs) vs d
  insertPT (Var v : f) (PT cs vs d) =
    PT cs (insert1 (fst (vm M.! v)) f vs) d
ptInsert _ _ _ = error "bad decl"

reorderMap :: M.Map Var Int -> Fmla -> [MMExpr] -> [MMExpr]
reorderMap m = \f -> let l = I.elems (buildIM f 0 I.empty) in \es -> map (es !!) l where
  buildIM :: Fmla -> Int -> I.IntMap Int -> I.IntMap Int
  buildIM [] _ i = i
  buildIM (Const _ : f) n i = buildIM f n i
  buildIM (Var v : f) n i = buildIM f (n+1) (I.insert (m M.! v) n i)

type ParseResult = [(([MMExpr] -> [MMExpr]) -> MMExpr, [Sym])]
parseFmla :: Fmla -> FromMMM (Const, MMExpr)
parseFmla = \case
  Const s : f -> do
    t <- get
    let s2 = fromMaybe s (fst $ mSorts (mDB t) M.! s)
    e <- fromJustError ("cannot parse formula " ++ show (Const s : f))
      (parseFmla' (mVMap t) (mParser t) s2 f)
    return (s, e)
  f -> throwError ("bad formula" ++ show f)
  where
  parseFmla' :: M.Map Var (Sort, Label) -> Parser -> Sort -> [Sym] -> Maybe MMExpr
  parseFmla' vm p = \s f -> parseEOF (parse s p f) where
    parseEOF :: [(MMExpr, [Sym])] -> Maybe MMExpr
    parseEOF [] = Nothing
    parseEOF ((e, []) : _) = Just e
    parseEOF (_ : es) = parseEOF es
    parse :: Sort -> Parser -> [Sym] -> [(MMExpr, [Sym])]
    parse s q f = (case f of
      Var v : f' -> let (s', v') = vm M.! v in
        if s == s' then [(SVar v', f')] else []
      _ -> []) ++ ((\(g, f') -> (g id, f')) <$> parseC s q s f)
    parseC :: Sort -> Parser -> Const -> [Sym] -> ParseResult
    parseC s q c f = parsePT s f (M.findWithDefault ptEmpty c q)
    parseD :: Sort -> Maybe (Sort, [MMExpr] -> MMExpr) -> [Sym] -> ParseResult
    parseD s (Just (s', i)) f | s' == s = [(\g -> i (g []), f)]
    parseD _ _ _ = []
    parsePT :: Sort -> [Sym] -> ParseTrie -> ParseResult
    parsePT s f (PT cs vs d) = (case f of
      Const c : f' -> parseC s cs c f'
      _ -> []) ++ parseV s vs f ++ parseD s d f
    parseV :: Sort -> M.Map Sort ParseTrie -> [Sym] -> ParseResult
    parseV s vs f = M.foldrWithKey parseV1 [] vs where
      parseV1 :: Sort -> ParseTrie -> ParseResult -> ParseResult
      parseV1 s' q r = (do
        (v, f') <- parse s' p f
        (g, f'') <- parsePT s f' q
        [(g . (. (v :)), f'')]) ++ r
