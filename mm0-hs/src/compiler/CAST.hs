module CAST (module CAST, AtDepType(..), SortData(..)) where

import qualified Data.Text as T
import Environment (SortData(..))

type Offset = Int
data AtPos a = AtPos Offset a deriving (Show)
data Span a = Span Offset a Offset deriving (Show)

instance Functor AtPos where
  fmap f (AtPos l a) = AtPos l (f a)

instance Functor Span where
  fmap f (Span l a r) = Span l (f a) r

unPos :: AtPos a -> a
unPos (AtPos _ a) = a

unSpan :: Span a -> a
unSpan (Span _ a _) = a

type AST = [AtPos Stmt]

data Visibility = Public | Abstract | Local | VisDefault deriving (Eq)
data DeclKind = DKTerm | DKAxiom | DKTheorem | DKDef deriving (Eq)
data Stmt =
    Sort Offset T.Text SortData
  | Decl Visibility DeclKind Offset T.Text
      [Binder] (Maybe [Type]) (Maybe LispVal)
  | Theorems [Binder] [LispVal]
  | Notation Notation
  | Inout Inout
  | Annot LispVal (AtPos Stmt)
  | Do [LispVal]

data Notation =
    Delimiter [Char] (Maybe [Char])
  | Prefix Offset T.Text Const Prec
  | Infix Bool Offset T.Text Const Prec
  | Coercion T.Text T.Text T.Text
  | NNotation T.Text [Binder] (Maybe Type) [Literal]

data Literal = NConst Const Prec | NVar T.Text

data Const = Const {cOffs :: Offset, cToken :: T.Text}
data Prec = Prec Int | PrecMax deriving (Eq)

instance Show Prec where
  show (Prec n) = show n
  show PrecMax = "max"

instance Ord Prec where
  _ <= PrecMax = True
  PrecMax <= _ = False
  Prec m <= Prec n = m <= n

type InputKind = T.Text
type OutputKind = T.Text

data Inout =
    Input InputKind [LispVal]
  | Output OutputKind [LispVal]

data Local = LBound T.Text | LReg T.Text | LDummy T.Text | LAnon

data AtDepType = AtDepType (AtPos T.Text) [AtPos T.Text]

data Formula = Formula Offset T.Text

data Type = TType AtDepType | TFormula Formula

tyOffset :: Type -> Offset
tyOffset (TType (AtDepType (AtPos o _) _)) = o
tyOffset (TFormula (Formula o _)) = o

data Binder = Binder Offset Local (Maybe Type)

isLBound :: Local -> Bool
isLBound (LBound _) = True
isLBound _ = False

isLCurly :: Local -> Bool
isLCurly (LBound _) = True
isLCurly (LDummy _) = True
isLCurly _ = False

localName :: Local -> Maybe T.Text
localName (LBound v) = Just v
localName (LReg v) = Just v
localName (LDummy v) = Just v
localName LAnon = Nothing

data LispVal =
    Atom T.Text
  | List [LispVal]
  | DottedList [LispVal] LispVal
  | Number Integer
  | String T.Text
  | Bool Bool
  | LFormula Formula

cons :: LispVal -> LispVal -> LispVal
cons l (List r) = List (l : r)
cons l (DottedList rs r) = DottedList (l : rs) r
cons l r = DottedList [l] r

lvLength :: LispVal -> Int
lvLength (DottedList e _) = length e
lvLength (List e) = length e
lvLength _ = 0

data LocalCtx = LocalCtx {

}
