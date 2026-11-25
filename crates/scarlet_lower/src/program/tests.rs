use super::*;
use scarlet_syntax::span::Span;
use scarlet_types::def::{FieldDef, VariantDef};
use scarlet_types::name::Name;
use scarlet_types::ty::TypeVar;

const NOWHERE: Span = Span {
    start_line: 0,
    start_column: 0,
    end_line: 0,
    end_column: 0,
};

/// The types of a program, by their names.
struct Defs(Vec<TypeDef>);

impl TypeDefs for Defs {
    fn type_def(&self, name: &TypeName) -> Option<&TypeDef> {
        self.0.iter().find(|def| def.name == *name)
    }
}

fn name(of: &str) -> TypeName {
    TypeName::new(
        ModuleKey::for_stdlib(&vec!["scarlet".to_string()]),
        of.into(),
    )
}

fn def(of: &str, body: TypeBody) -> TypeDef {
    TypeDef {
        name: name(of),
        params: Vec::new(),
        body,
        public: false,
        doc: None,
        span: NOWHERE,
    }
}

/// A type that declares a constructor with so many fields for each of `fields`.
fn variants(of: &str, fields: &[usize], constructors_public: bool) -> TypeDef {
    let int = Type::named(Known::new().int, Vec::new());
    let field = |i: usize| FieldDef {
        label: Name::from(format!("f{i}").as_str()),
        ty: int.clone(),
        span: NOWHERE,
    };
    let variants = fields.iter().zip(0..).map(|(n, i)| VariantDef {
        name: Name::from(format!("V{i}").as_str()),
        fields: (0..*n).map(field).collect(),
        doc: None,
        span: NOWHERE,
    });
    let body = TypeBody::Variants {
        variants: variants.collect(),
        constructors_public,
        variance: Vec::new(),
    };
    def(of, body)
}

fn external(of: &str) -> TypeDef {
    let variance = Vec::new();
    def(of, TypeBody::External { variance })
}

fn named(of: &str, args: &[Type]) -> Type {
    Type::named(name(of), args.to_vec())
}

/// A type is a word when each of its constructors has no fields and there is
/// one, whoever may name them and whatever its arguments. A Float is one too.
/// Nothing else is, an Int and what the host backs among it.
#[test]
fn a_type_is_a_word_by_its_constructors() {
    let defs = Defs(vec![
        variants("One", &[0], true),
        variants("Many", &[0; 300], true),
        variants("Opaque", &[0, 0], false),
        variants("Mixed", &[0, 1, 0], true),
        variants("Last", &[0, 0, 1], true),
        variants("Record", &[3], true),
        variants("None", &[], true),
        external("Int"),
        external("Float"),
        external("String"),
        external("Array"),
        external("Map"),
        external("Pid"),
    ]);
    let mut ids = Ids::new(&defs);
    let var = Type::Var(TypeVar {
        id: 0,
        name: None,
        constraint: None,
    });
    let one = named("One", &[]);
    let words = [
        one.clone(),
        named("Many", &[]),
        named("Opaque", &[]),
        named("One", std::slice::from_ref(&var)),
        named("Many", &[named("Record", &[])]),
        named("Float", &[]),
    ];
    let counted = [
        named("Mixed", &[]),
        named("Last", &[]),
        named("Record", &[]),
        named("Record", std::slice::from_ref(&one)),
        named("None", &[]),
        named("Int", &[]),
        named("String", &[]),
        named("Array", std::slice::from_ref(&one)),
        named("Map", &[one.clone(), one.clone()]),
        named("Pid", &[]),
        var,
        Type::tuple(vec![one.clone(), one.clone()]),
        Type::tuple(Vec::new()),
        Type::function(Vec::new(), one.clone()),
        Type::function(vec![one.clone()], one),
    ];
    for (types, repr) in [(&words[..], Repr::Word), (&counted[..], Repr::Counted)] {
        for t in types {
            let rty = ids.rty(t).unwrap();
            assert_eq!(ids.pool().repr(rty), Ok(repr), "{t}");
            // It is the same when it is asked for again.
            assert_eq!(ids.rty(t), Ok(rty), "{t}");
        }
    }
}

/// A type that nothing defines, or that is another name for one, is an error
/// wherever it stands in another type, and takes no id.
#[test]
fn a_type_that_nothing_says_anything_of_is_an_error() {
    let alias = def("Alias", TypeBody::Alias(named("One", &[])));
    let defs = Defs(vec![variants("One", &[0], true), alias]);
    let mut ids = Ids::new(&defs);
    for missing in ["Lost", "Alias"] {
        let t = named(missing, &[]);
        let why = Err(InternalLinkError::NoRepr(name(missing)).into());
        let all = [
            t.clone(),
            named("One", std::slice::from_ref(&t)),
            Type::tuple(vec![named("One", &[]), t.clone()]),
            Type::function(vec![t.clone()], named("One", &[])),
            Type::function(Vec::new(), t),
        ];
        for t in all {
            assert_eq!(ids.rty(&t), why, "{t}");
        }
        assert_eq!(ids.ctor_ref(&name(missing), 0).err(), why.err());
    }
    let one = ids.type_id(&name("One")).unwrap();
    let mut other = Ids::new(&defs);
    assert_eq!(other.type_id(&name("One")), Ok(one));
    let why = InternalLinkError::NoRepr(name("Lost"));
    let text = "internal compiler error: nothing says what a value of `Lost` is";
    assert_eq!(LinkError::from(why).to_string(), text);
}
