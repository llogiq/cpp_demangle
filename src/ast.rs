//! Abstract syntax tree types for mangled symbols.

extern crate fixedbitset;

use error::{self, Result};
use index_str::IndexStr;
use self::fixedbitset::FixedBitSet;
#[cfg(feature = "logging")]
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::io::{self, Write};
use subs::{Substitutable, SubstitutionTable};

struct AutoLogParse;

thread_local! {
    #[cfg(feature = "logging")]
    static PARSE_DEPTH: RefCell<usize> = RefCell::new(0);
}

impl AutoLogParse {
    #[cfg(feature = "logging")]
    fn new<'a>(production: &'static str, input: IndexStr<'a>) -> AutoLogParse {
        PARSE_DEPTH.with(|depth| {
            if *depth.borrow() == 0 {
                println!("");
            }

            let indent: String = (0..*depth.borrow() * 4).map(|_| ' ').collect();
            log!("{}({} \"{}\"",
                 indent,
                 production,
                 String::from_utf8_lossy(input.as_ref()));
            *depth.borrow_mut() += 1;
        });
        AutoLogParse
    }

    #[cfg(not(feature = "logging"))]
    #[inline(always)]
    fn new<'a>(_: &'static str, _: IndexStr<'a>) -> AutoLogParse {
        AutoLogParse
    }
}

#[cfg(feature = "logging")]
impl Drop for AutoLogParse {
    fn drop(&mut self) {
        PARSE_DEPTH.with(|depth| {
            *depth.borrow_mut() -= 1;
            let indent: String = (0..*depth.borrow() * 4).map(|_| ' ').collect();
            log!("{})", indent);
        });
    }
}

/// Automatically log start and end parsing in an s-expression format, when the
/// `logging` feature is enabled.
macro_rules! log_parse {
    ( $production:expr , $input:expr ) => {
        let _log = AutoLogParse::new($production, $input);
    }
}

/// A trait for anything that can be parsed from an `IndexStr` and return a
/// `Result` of the parsed `Self` value and the rest of the `IndexStr` input
/// that has not been consumed in parsing the `Self` value.
///
/// For AST types representing productions which have `<substitution>` as a
/// possible right hand side, do not implement this trait directly. Instead,
/// make a newtype over `usize`, parse either the `<substitution>` back
/// reference or "real" value, insert the "real" value into the substitution
/// table if needed, and *always* return the newtype index into the substitution
/// table.
#[doc(hidden)]
pub trait Parse: Sized {
    /// Parse the `Self` value from `input` and return it, updating the
    /// substitution table as needed.
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Self, IndexStr<'b>)>;
}

/// A trait to abstract looking ahead one byte during parsing.
trait StartsWith {
    /// Does this production start with the given byte?
    fn starts_with(byte: u8) -> bool;
}

/// Determine whether this AST node is an instantiated[*] template function, and
/// get its concrete template arguments.
///
/// [*] Note that we will never see an abstract, un-instantiated template
/// function, since they don't end up in object files and don't get mangled
/// names.
trait GetTemplateArgs {
    /// Returns `Some` if this is a template function, `None` otherwise.
    fn get_template_args<'a>(&'a self,
                             subs: &'a SubstitutionTable)
                             -> Option<&'a TemplateArgs>;
}

/// When formatting a mangled symbol's parsed AST as a demangled symbol, we need
/// to resolve indirect references to template and function arguments with
/// direct `TemplateArg` and `Type` references respectively.
///
/// Note that which set of arguments are implicitly referenced change as we
/// enter and leave different functions' scope. One might usually use de Brujin
/// indices to keep arguments within scopes separated from each other, but the
/// Itanium C++ ABI does not allow us the luxury. AFAIK, when the ABI was first
/// drafted, C++ did not have lambdas, and the issue did not come up at all
/// since a function simply couldn't refer to the types of closed over
/// variables.
///
/// This trait is implemented by anything that can potentially resolve arguments
/// for us.
trait ArgResolver: fmt::Debug {
    /// Get the current context's `idx`th template argument.
    fn get_template_arg(&self, idx: usize) -> Result<&TemplateArg>;

    /// Get the current context's `idx`th function argument's type.
    fn get_function_arg(&self, idx: usize) -> Result<&Type>;
}

/// An `ArgStack` represents the current function and template demangling scope
/// we are within. As we enter new demangling scopes, we construct new
/// `ArgStack`s whose `prev` references point back to the old ones. These
/// `ArgStack`s are kept on the native stack, and as functions return, they go
/// out of scope and we use the previous `ArgStack`s again.
#[derive(Copy, Clone, Debug)]
pub struct ArgStack<'a, 'b> {
    prev: Option<&'a ArgStack<'a, 'a>>,
    item: &'b ArgResolver,
}

/// When we first begin demangling, we haven't entered any function or template
/// demangling scope and we don't have any useful `ArgStack`. Therefore, we are
/// never actually dealing with `ArgStack` directly in practice, but always an
/// `Option<ArgStack>` instead. Nevertheless, we want to define useful methods
/// on `Option<ArgStack>`.
///
/// A custom "extension" trait with exactly one implementor: Rust's principled
/// monkey patching!
trait ArgStackExt<'a, 'b> {
    /// Push a new `ArgResolver` onto this `ArgStack` and return the new
    /// `ArgStack` with the pushed resolver on top.
    fn push(&'a self, item: &'b ArgResolver) -> Option<ArgStack<'a, 'b>>;
}

impl<'a, 'b> ArgStackExt<'a, 'b> for Option<ArgStack<'a, 'a>> {
    fn push(&'a self, item: &'b ArgResolver) -> Option<ArgStack<'a, 'b>> {
        Some(ArgStack {
            prev: self.as_ref(),
            item: item,
        })
    }
}

/// A stack of `ArgResolver`s is itself an `ArgResolver`!
impl<'a, 'b> ArgResolver for Option<ArgStack<'a, 'b>> {
    fn get_template_arg(&self, idx: usize) -> Result<&TemplateArg> {
        let mut stack = *self;
        while let Some(s) = stack {
            if let Ok(arg) = s.item.get_template_arg(idx) {
                return Ok(arg);
            }
            stack = s.prev.cloned();
        }

        Err(error::Error::BadTemplateArgReference)
    }

    fn get_function_arg(&self, idx: usize) -> Result<&Type> {
        let mut stack = *self;
        while let Some(s) = stack {
            if let Ok(arg) = s.item.get_function_arg(idx) {
                return Ok(arg);
            }
            stack = s.prev.cloned();
        }

        Err(error::Error::BadFunctionArgReference)
    }
}

/// Common state that is required when demangling a mangled symbol's parsed AST.
#[doc(hidden)]
#[derive(Debug)]
pub struct DemangleContext<'a, W>
    where W: io::Write
{
    // The substitution table built up when parsing the mangled symbol into an
    // AST.
    subs: &'a SubstitutionTable,

    // The original input string.
    input: &'a [u8],

    // What the demangled name is being written to.
    out: W,

    // The total number of bytes written to `out`. This is maintained by the
    // `Write` implementation for `DemangleContext`.
    bytes_written: usize,

    // The last byte written to `out`, if any.
    last_byte_written: Option<u8>,

    // Any time we start demangling an entry from the substitutions table, we
    // mark its corresponding bit here. Before we begin demangling such an
    // entry, we check whether the bit is set. If it is set, then we have
    // entered a substitutions reference cycle and will go into a infinite
    // recursion and blow the stack.
    //
    // TODO: is this really needed? Shouldn't the check that back references are
    // always backwards mean that there can't be cycles? Alternatively, is that
    // check too strict, and should it be relaxed?
    mark_bits: FixedBitSet,
}

impl<'a, W> io::Write for DemangleContext<'a, W>
    where W: io::Write
{
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        self.out.write(buf).map(|n| {
            self.last_byte_written = buf.last().cloned();
            self.bytes_written += n;
            n
        })
    }

    fn flush(&mut self) -> io::Result<()> {
        self.out.flush()
    }
}

impl<'a, W> DemangleContext<'a, W>
    where W: io::Write
{
    /// Construct a new `DemangleContext`.
    pub fn new(subs: &'a SubstitutionTable,
               input: &'a [u8],
               out: W)
               -> DemangleContext<'a, W> {
        DemangleContext {
            subs: subs,
            input: input,
            out: out,
            bytes_written: 0,
            last_byte_written: None,
            mark_bits: FixedBitSet::with_capacity(subs.len()),
        }
    }

    fn set_mark_bit(&mut self, idx: usize) {
        self.mark_bits.set(idx, true);
    }

    fn clear_mark_bit(&mut self, idx: usize) {
        self.mark_bits.set(idx, false);
    }

    fn mark_bit_is_set(&self, idx: usize) -> bool {
        self.mark_bits[idx]
    }

    fn ensure_space(&mut self) -> io::Result<()> {
        if let Some(b' ') = self.last_byte_written {
            Ok(())
        } else {
            try!(write!(self, " "));
            Ok(())
        }
    }
}

/// Any AST node that can be printed in a demangled form.
#[doc(hidden)]
pub trait Demangle {
    /// Write the demangled form of this AST node to the given context.
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write;
}

impl Demangle for str {
    #[inline(always)]
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   _: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "{}", self));
        Ok(())
    }
}

/// Sometimes an AST node needs to insert itself as an inner item within one of
/// its children when demangling that child. For example, the AST `(array 10
/// int)` is demangled as `int[10]`, but if we were to demangle `(lvalue-ref
/// (array 10 int))` then we want this demangled form: `int (&) [10]`. The
/// `DemangleWithInner` trait enables such behavior by allowing us to pass AST
/// parents down to their children as inner items.
///
/// The inner item is an `Option` so we can provide a default `Demangle`
/// implementation for all `DemangleWithInner` implementors, and don't have to
/// write two copies of almost-but-not-quite the same code.
pub trait DemangleWithInner {
    /// Demangle this type with the given inner item.
    fn demangle_with_inner<D, W>(&self,
                                 inner: Option<&D>,
                                 ctx: &mut DemangleContext<W>,
                                 stack: Option<ArgStack>)
                                 -> io::Result<()>
        where D: ?Sized + Demangle,
              W: io::Write;
}

impl<D> Demangle for D
    where D: DemangleWithInner
{
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        let inner: Option<&str> = None;
        self.demangle_with_inner(inner, ctx, stack)
    }
}

/// Demangle T and concatenate it with the demangling of U.
struct Concat<'a, 'b, T, U>(&'a T, &'b U)
    where T: 'a + ?Sized,
          U: 'b + ?Sized;

impl<'a, 'b, T, U> Demangle for Concat<'a, 'b, T, U>
    where T: 'a + ?Sized + Demangle,
          U: 'b + ?Sized + Demangle
{
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(self.0.demangle(ctx, stack));
        self.1.demangle(ctx, stack)
    }
}

struct FunctionArgList<'a>(&'a [TypeHandle]);

impl<'a> Demangle for FunctionArgList<'a> {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "("));

        // To maintain compatibility with libiberty, print `()` instead
        // of `(void)` for functions that take no arguments.
        if self.0.len() == 1 && self.0[0].is_void() {
            try!(write!(ctx, ")"));
            return Ok(());
        }

        let mut need_comma = false;
        for arg in self.0 {
            if need_comma {
                try!(write!(ctx, ", "));
            }
            try!(arg.demangle(ctx, stack));
            need_comma = true;
        }

        try!(write!(ctx, ")"));
        Ok(())
    }
}

/// Define a handle to a AST type that lives inside the substitution table. A
/// handle is always either an index into the substitution table, or it is a
/// reference to a "well-known" component.
///
/// This declares:
///
/// - The enum of either a back reference into the substitution table or a
///   reference to a "well-known" component
/// - a `Demangle` impl that proxies to the appropriate `Substitutable` in the
///   `SubstitutionTable`
macro_rules! define_handle {
    (
        $(#[$attr:meta])*
        pub enum $typename:ident
    ) => {
        define_handle! {
            $(#[$attr])*
            pub enum $typename {}
        }
    };

    (
        $(#[$attr:meta])*
        pub enum $typename:ident {
            $(
                $( #[$extra_attr:meta] )*
                extra $extra_variant:ident ( $extra_variant_ty:ident ),
            )*
        }
    ) => {
        $(#[$attr])*
        #[derive(Clone, Debug, Hash, PartialEq, Eq)]
        pub enum $typename {
            /// A reference to a "well-known" component.
            WellKnown(WellKnownComponent),

            /// A back-reference into the substitution table to a component we
            /// have already parsed.
            BackReference(usize),

            $(
                $( #[$extra_attr] )*
                $extra_variant( $extra_variant_ty ),
            )*
        }

        impl $typename {
            /// If this is a `BackReference`, get its index.
            pub fn back_reference(&self) -> Option<usize> {
                match *self {
                    $typename::BackReference(n) => Some(n),
                    _ => None,
                }
            }
        }

        impl Demangle for $typename {
            fn demangle<W>(&self,
                           ctx: &mut DemangleContext<W>, stack: Option<ArgStack>)
                           -> io::Result<()>
                where W: io::Write
            {
                match *self {
                    $typename::WellKnown(ref comp) => comp.demangle(ctx, stack),
                    $typename::BackReference(idx) => {
                        if ctx.mark_bit_is_set(idx) {
                            return Err(io::Error::new(io::ErrorKind::Other,
                                                      error::Error::RecursiveDemangling.description()));
                        }

                        ctx.set_mark_bit(idx);
                        let ret = ctx.subs[idx].demangle(ctx, stack);
                        ctx.clear_mark_bit(idx);
                        ret
                    }
                    $(
                        $typename::$extra_variant(ref extra) => extra.demangle(ctx, stack),
                    )*
                }
            }
        }
    };
}

/// Define a "vocabulary" nonterminal, something like `OperatorName` or
/// `CtorDtorName` that's basically a big list of constant strings.
///
/// This declares:
///
/// - the enum itself
/// - a `Parse` impl
/// - a `StartsWith` impl
/// - a `Demangle` impl
///
/// See the definition of `CTorDtorName` for an example of its use.
macro_rules! define_vocabulary {
    ( $(#[$attr:meta])* pub enum $typename:ident {
        $($variant:ident ( $mangled:expr, $printable:expr )),*
    } ) => {

        $(#[$attr])*
        pub enum $typename {
            $(
                #[doc=$printable]
                $variant
            ),*
        }

        impl Parse for $typename {
            fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                             input: IndexStr<'b>)
                             -> Result<($typename, IndexStr<'b>)> {
                log_parse!(stringify!($typename), input);

                let mut found_prefix = false;
                $(
                    if let Some((head, tail)) = input.try_split_at($mangled.len()) {
                        if head.as_ref() == $mangled {
                            return Ok(($typename::$variant, tail));
                        }
                    } else {
                        found_prefix |= 0 < input.len() &&
                            input.len() < $mangled.len() &&
                            input.as_ref() == &$mangled[..input.len()];
                    }
                )*

                if input.is_empty() || found_prefix {
                    Err(error::Error::UnexpectedEnd)
                } else {
                    Err(error::Error::UnexpectedText)
                }
            }
        }

        impl Demangle for $typename {
            fn demangle<W>(&self,
                           ctx: &mut DemangleContext<W>, _: Option<ArgStack>)
                           -> io::Result<()>
                where W: io::Write
            {
                write!(ctx, "{}", match *self {
                    $(
                        $typename::$variant => $printable
                    ),*
                })
            }
        }

        impl StartsWith for $typename {
            #[inline]
            fn starts_with(byte: u8) -> bool {
                $(
                    if $mangled[0] == byte {
                        return true;
                    }
                )*

                false
            }
        }
    }
}

/// The root AST node, and starting production.
///
/// ```text
/// <mangled-name> ::= _Z <encoding>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum MangledName {
    /// The encoding of the mangled symbol name.
    Encoding(Encoding),

    /// A top-level type. Technically not allowed by the standard, however in
    /// practice this can happen, and is tested for by libiberty.
    Type(TypeHandle),
}

impl Parse for MangledName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(MangledName, IndexStr<'b>)> {
        log_parse!("MangledName", input);

        // The _Z from the spec is really just a suggestion... Sometimes there
        // is an extra leading underscore (like what we get out of `nm`) and
        // sometimes it appears to be completely missing, if libiberty tests are
        // to be trusted...
        let tail = if let Ok(tail) = consume(b"__Z", input) {
            tail
        } else {
            if let Ok(tail) = consume(b"_Z", input) {
                tail
            } else {
                input
            }
        };

        if let Ok((encoding, tail)) = Encoding::parse(subs, tail) {
            return Ok((MangledName::Encoding(encoding), tail))
        };

        // The libiberty tests also specify that a type can be top level.
        let (ty, tail) = try!(TypeHandle::parse(subs, input));
        Ok((MangledName::Type(ty), tail))
    }
}

impl Demangle for MangledName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            MangledName::Encoding(ref enc)=> enc.demangle(ctx, stack),
            MangledName::Type(ref ty) => ty.demangle(ctx, stack),
        }
    }
}

/// The `<encoding>` production.
///
/// ```text
/// <encoding> ::= <function name> <bare-function-type>
///            ::= <data name>
///            ::= <special-name>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Encoding {
    /// An encoded function.
    Function(Name, BareFunctionType),

    /// An encoded static variable.
    Data(Name),

    /// A special encoding.
    Special(SpecialName),
}

impl Parse for Encoding {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Encoding, IndexStr<'b>)> {
        log_parse!("Encoding", input);

        if let Ok((name, tail)) = Name::parse(subs, input) {
            if let Ok((ty, tail)) = BareFunctionType::parse(subs, tail) {
                return Ok((Encoding::Function(name, ty), tail));
            } else {
                return Ok((Encoding::Data(name), tail));
            }
        }

        let (name, tail) = try!(SpecialName::parse(subs, input));
        Ok((Encoding::Special(name), tail))
    }
}

impl Demangle for Encoding {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            Encoding::Function(ref name, ref fun_ty) => {
                // Even if this function takes no args and doesn't have a return
                // value (see below), it will have the void parameter.
                debug_assert!(fun_ty.0.len() >= 1);

                // Whether the first type in the BareFunctionType is a return
                // type or parameter depends on the context in which it
                // appears.
                //
                // * Templates and functions in a type or parameter position
                // always have return types.
                //
                // * Non-template functions that are not in a type or parameter
                // position do not have a return type.
                //
                // We know we are not printing a type, so we only need to check
                // whether this is a template.
                //
                // For the details, see
                // http://mentorembedded.github.io/cxx-abi/abi.html#mangle.function-type
                // let args = if name.is_template_function(ctx.subs) {
                //     try!(fun_ty.0[0].demangle(ctx));
                let (stack, function_args) = if let Some(template_args) =
                    name.get_template_args(ctx.subs) {
                    let stack = stack.push(template_args);
                    let function_args = FunctionArgList(&fun_ty.0[1..]);

                    try!(fun_ty.0[0].demangle(ctx, stack));
                    try!(write!(ctx, " "));

                    (stack, function_args)
                } else {
                    (stack, FunctionArgList(&fun_ty.0[..]))
                };

                if let Name::Nested(ref name) = *name {
                    return name.demangle_with_inner(Some(&function_args), ctx, stack);
                }

                try!(name.demangle(ctx, stack));
                function_args.demangle(ctx, stack)
            }
            Encoding::Data(ref name) => name.demangle(ctx, stack),
            Encoding::Special(ref name) => name.demangle(ctx, stack),
        }
    }
}

/// The `<name>` production.
///
/// ```text
/// <name> ::= <nested-name>
///        ::= <unscoped-name>
///        ::= <unscoped-template-name> <template-args>
///        ::= <local-name>
///        ::= St <unqualified-name> # ::std::
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Name {
    /// A nested name
    Nested(NestedName),

    /// An unscoped name.
    Unscoped(UnscopedName),

    /// An unscoped template.
    UnscopedTemplate(UnscopedTemplateNameHandle, TemplateArgs),

    /// A local name.
    Local(LocalName),

    /// A name in `::std::`.
    Std(UnqualifiedName),
}

impl Parse for Name {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Name, IndexStr<'b>)> {
        log_parse!("Name", input);

        if let Ok((name, tail)) = NestedName::parse(subs, input) {
            return Ok((Name::Nested(name), tail));
        }

        if let Ok(tail) = consume(b"St", input) {
            let (name, tail) = try!(UnqualifiedName::parse(subs, tail));
            return Ok((Name::Std(name), tail));
        }

        if let Ok((name, tail)) = UnscopedName::parse(subs, input) {
            if tail.peek() == Some(b'I') {
                let name = UnscopedTemplateName(name);
                let idx = subs.insert(Substitutable::UnscopedTemplateName(name));
                let handle = UnscopedTemplateNameHandle::BackReference(idx);

                let (args, tail) = try!(TemplateArgs::parse(subs, tail));
                return Ok((Name::UnscopedTemplate(handle, args), tail));
            } else {
                return Ok((Name::Unscoped(name), tail));
            }
        }

        if let Ok((name, tail)) = UnscopedTemplateNameHandle::parse(subs, input) {
            let (args, tail) = try!(TemplateArgs::parse(subs, tail));
            return Ok((Name::UnscopedTemplate(name, args), tail));
        }

        let (name, tail) = try!(LocalName::parse(subs, input));
        Ok((Name::Local(name), tail))
    }
}

impl Demangle for Name {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            Name::Nested(ref nested) => nested.demangle(ctx, stack),
            Name::Unscoped(ref unscoped) => unscoped.demangle(ctx, stack),
            Name::UnscopedTemplate(ref template, ref args) => {
                try!(template.demangle(ctx, stack.push(args)));
                args.demangle(ctx, stack)
            }
            Name::Local(ref local) => local.demangle(ctx, stack),
            Name::Std(ref std) => {
                try!(write!(ctx, "std::"));
                std.demangle(ctx, stack)
            }
        }
    }
}

impl GetTemplateArgs for Name {
    fn get_template_args<'a>(&'a self,
                             subs: &'a SubstitutionTable)
                             -> Option<&'a TemplateArgs> {
        match *self {
            Name::UnscopedTemplate(_, ref args) => Some(args),
            Name::Nested(ref nested) => nested.get_template_args(subs),
            Name::Local(ref local) => local.get_template_args(subs),
            Name::Unscoped(_) |
            Name::Std(_) => None,
        }
    }
}

/// The `<unscoped-name>` production.
///
/// ```text
/// <unscoped-name> ::= <unqualified-name>
///                 ::= St <unqualified-name>   # ::std::
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum UnscopedName {
    /// An unqualified name.
    Unqualified(UnqualifiedName),

    /// A name within the `std::` namespace.
    Std(UnqualifiedName),
}

impl Parse for UnscopedName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnscopedName, IndexStr<'b>)> {
        log_parse!("UnscopedName", input);

        if let Ok(tail) = consume(b"St", input) {
            let (name, tail) = try!(UnqualifiedName::parse(subs, tail));
            return Ok((UnscopedName::Std(name), tail));
        }

        let (name, tail) = try!(UnqualifiedName::parse(subs, input));
        Ok((UnscopedName::Unqualified(name), tail))
    }
}

impl Demangle for UnscopedName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            UnscopedName::Unqualified(ref unqualified) => {
                unqualified.demangle(ctx, stack)
            }
            UnscopedName::Std(ref std) => {
                try!(write!(ctx, "std::"));
                std.demangle(ctx, stack)
            }
        }
    }
}

/// The `<unscoped-template-name>` production.
///
/// ```text
/// <unscoped-template-name> ::= <unscoped-name>
///                          ::= <substitution>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UnscopedTemplateName(UnscopedName);

define_handle! {
    /// A handle to an `UnscopedTemplateName`.
    pub enum UnscopedTemplateNameHandle
}

impl Parse for UnscopedTemplateNameHandle {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnscopedTemplateNameHandle, IndexStr<'b>)> {
        log_parse!("UnscopedTemplateNameHandle", input);

        if let Ok((name, tail)) = UnscopedName::parse(subs, input) {
            let name = UnscopedTemplateName(name);
            let idx = subs.insert(Substitutable::UnscopedTemplateName(name));
            let handle = UnscopedTemplateNameHandle::BackReference(idx);
            return Ok((handle, tail));
        }

        let (sub, tail) = try!(Substitution::parse(subs, input));

        match sub {
            Substitution::WellKnown(component) => {
                Ok((UnscopedTemplateNameHandle::WellKnown(component), tail))
            }
            Substitution::BackReference(idx) => {
                // TODO: should this check/assert that subs[idx] is an
                // UnscopedTemplateName?
                Ok((UnscopedTemplateNameHandle::BackReference(idx), tail))
            }
        }
    }
}

impl Demangle for UnscopedTemplateName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        self.0.demangle(ctx, stack)
    }
}

/// The `<nested-name>` production.
///
/// ```text
/// <nested-name> ::= N [<CV-qualifiers>] [<ref-qualifier>] <prefix> <unqualified-name> E
///               ::= N [<CV-qualifiers>] [<ref-qualifier>] <template-prefix> <template-args> E
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct NestedName(CvQualifiers, Option<RefQualifier>, PrefixHandle);

impl Parse for NestedName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(NestedName, IndexStr<'b>)> {
        log_parse!("NestedName", input);

        let tail = try!(consume(b"N", input));

        let (cv_qualifiers, tail) = if let Ok((q, tail)) = CvQualifiers::parse(subs,
                                                                               tail) {
            (q, tail)
        } else {
            (Default::default(), tail)
        };

        let (ref_qualifier, tail) = if let Ok((r, tail)) = RefQualifier::parse(subs,
                                                                               tail) {
            (Some(r), tail)
        } else {
            (None, tail)
        };

        let (prefix, tail) = try!(PrefixHandle::parse(subs, tail));
        if let PrefixHandle::BackReference(idx) = prefix {
            match (*subs)[idx] {
                // The <nested-name> must end with one of these kinds of prefix
                // components.
                Substitutable::Prefix(Prefix::Nested(..)) |
                Substitutable::Prefix(Prefix::Template(..)) => {}
                _ => return Err(error::Error::UnexpectedText),
            }
        }

        let tail = try!(consume(b"E", tail));
        Ok((NestedName(cv_qualifiers, ref_qualifier, prefix), tail))
    }
}

impl DemangleWithInner for NestedName {
    fn demangle_with_inner<D, W>(&self,
                                 inner: Option<&D>,
                                 ctx: &mut DemangleContext<W>,
                                 stack: Option<ArgStack>)
                                 -> io::Result<()>
        where D: ?Sized + Demangle,
              W: io::Write
    {
        try!(self.2.demangle(ctx, stack));

        if let Some(inner) = inner {
            try!(inner.demangle(ctx, stack));
        }

        if self.0 != CvQualifiers::default() {
            try!(self.0.demangle(ctx, stack));
        }

        if let Some(ref refs) = self.1 {
            try!(ctx.ensure_space());
            try!(refs.demangle(ctx, stack));
        }

        Ok(())
    }
}

impl GetTemplateArgs for NestedName {
    fn get_template_args<'a>(&'a self,
                             subs: &'a SubstitutionTable)
                             -> Option<&'a TemplateArgs> {
        self.2.get_template_args(subs)
    }
}

/// The `<prefix>` production.
///
/// ```text
/// <prefix> ::= <unqualified-name>
///          ::= <prefix> <unqualified-name>
///          ::= <template-prefix> <template-args>
///          ::= <template-param>
///          ::= <decltype>
///          ::= <prefix> <data-member-prefix>
///          ::= <substitution>
///
/// <template-prefix> ::= <template unqualified-name>
///                   ::= <prefix> <template unqualified-name>
///                   ::= <template-param>
///                   ::= <substitution>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Prefix {
    /// An unqualified name.
    Unqualified(UnqualifiedName),

    /// Some nested name.
    Nested(PrefixHandle, UnqualifiedName),

    /// A prefix and template arguments.
    Template(PrefixHandle, TemplateArgs),

    /// A template parameter.
    TemplateParam(TemplateParam),

    /// A decltype.
    Decltype(Decltype),

    /// A prefix and data member.
    DataMember(PrefixHandle, DataMemberPrefix),
}

impl GetTemplateArgs for Prefix {
    fn get_template_args<'a>(&'a self,
                             _: &'a SubstitutionTable)
                             -> Option<&'a TemplateArgs> {
        match *self {
            Prefix::Template(_, ref args) => Some(args),
            Prefix::Unqualified(_) |
            Prefix::Nested(_, _) |
            Prefix::TemplateParam(_) |
            Prefix::Decltype(_) |
            Prefix::DataMember(_, _) => None,
        }
    }
}

define_handle! {
    /// A reference to a parsed `<prefix>` production.
    pub enum PrefixHandle
}

impl Parse for PrefixHandle {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(PrefixHandle, IndexStr<'b>)> {
        log_parse!("PrefixHandle", input);

        fn add_to_subs(subs: &mut SubstitutionTable, prefix: Prefix) -> PrefixHandle {
            let idx = subs.insert(Substitutable::Prefix(prefix));
            PrefixHandle::BackReference(idx)
        }

        let mut tail = input;
        let mut current = None;

        loop {
            log_parse!("PrefixHandle iteration", tail);

            match tail.peek() {
                None => {
                    if let Some(handle) = current {
                        return Ok((handle, tail));
                    } else {
                        return Err(error::Error::UnexpectedEnd);
                    }
                }
                Some(b'S') => {
                    // <prefix> ::= <substitution>
                    let (sub, tail_tail) = try!(Substitution::parse(subs, tail));
                    current = Some(match sub {
                        Substitution::WellKnown(component) => {
                            PrefixHandle::WellKnown(component)
                        }
                        Substitution::BackReference(idx) => {
                            // TODO: do we need to check that the idx actually points to
                            // a Prefix?
                            PrefixHandle::BackReference(idx)
                        }
                    });
                    tail = tail_tail;
                }
                Some(b'T') => {
                    // <prefix> ::= <template-param>
                    let (param, tail_tail) = try!(TemplateParam::parse(subs, tail));
                    current = Some(add_to_subs(subs, Prefix::TemplateParam(param)));
                    tail = tail_tail;
                }
                Some(b'D') => {
                    // Either
                    //
                    //     <prefix> ::= <decltype>
                    //
                    // or
                    //
                    //     <prefix> ::= <unqualified-name> ::= <ctor-dtor-name>
                    if let Ok((decltype, tail_tail)) = Decltype::parse(subs, tail) {
                        current = Some(add_to_subs(subs, Prefix::Decltype(decltype)));
                        tail = tail_tail;
                    } else {
                        let (name, tail_tail) = try!(UnqualifiedName::parse(subs, tail));
                        let prefix = match current {
                            None => Prefix::Unqualified(name),
                            Some(handle) => Prefix::Nested(handle, name),
                        };
                        current = Some(add_to_subs(subs, prefix));
                        tail = tail_tail;
                    }
                }
                Some(b'I') if current.is_some() &&
                              current.as_ref().unwrap().is_template_prefix(subs) => {
                    // <prefix> ::= <template-prefix> <template-args>
                    let (args, tail_tail) = try!(TemplateArgs::parse(subs, tail));
                    let prefix = Prefix::Template(current.unwrap(), args);
                    current = Some(add_to_subs(subs, prefix));
                    tail = tail_tail;
                }
                Some(c) if current.is_some() && SourceName::starts_with(c) => {
                    // Either
                    //
                    //     <prefix> ::= <unqualified-name> ::= <source-name>
                    //
                    // or
                    //
                    //     <prefix> ::= <data-member-prefix> ::= <prefix> <source-name> M
                    debug_assert!(UnqualifiedName::starts_with(c));
                    debug_assert!(DataMemberPrefix::starts_with(c));

                    let (name, tail_tail) = try!(SourceName::parse(subs, tail));
                    if tail_tail.peek() == Some(b'M') {
                        let prefix = Prefix::DataMember(current.unwrap(),
                                                        DataMemberPrefix(name));
                        current = Some(add_to_subs(subs, prefix));
                        tail = consume(b"M", tail_tail).unwrap();
                    } else {
                        let name = UnqualifiedName::Source(name);
                        let prefix = match current {
                            None => Prefix::Unqualified(name),
                            Some(handle) => Prefix::Nested(handle, name),
                        };
                        current = Some(add_to_subs(subs, prefix));
                        tail = tail_tail;
                    }
                }
                Some(c) if UnqualifiedName::starts_with(c) => {
                    // <prefix> ::= <unqualified-name>
                    let (name, tail_tail) = try!(UnqualifiedName::parse(subs, tail));
                    let prefix = match current {
                        None => Prefix::Unqualified(name),
                        Some(handle) => Prefix::Nested(handle, name),
                    };
                    current = Some(add_to_subs(subs, prefix));
                    tail = tail_tail;
                }
                Some(_) => {
                    if let Some(handle) = current {
                        return Ok((handle, tail));
                    } else if tail.is_empty() {
                        return Err(error::Error::UnexpectedEnd);
                    } else {
                        return Err(error::Error::UnexpectedText);
                    }
                }
            }
        }
    }
}

impl GetTemplateArgs for PrefixHandle {
    fn get_template_args<'a>(&'a self,
                             subs: &'a SubstitutionTable)
                             -> Option<&'a TemplateArgs> {
        match *self {
            PrefixHandle::BackReference(idx) => {
                if let Some(&Substitutable::Prefix(ref p)) = subs.get(idx) {
                    p.get_template_args(subs)
                } else {
                    None
                }
            }
            _ => None,
        }
    }
}

impl Prefix {
    // Is this <prefix> also a valid <template-prefix> production? Not to be
    // confused with the `GetTemplateArgs` trait.
    fn is_template_prefix(&self) -> bool {
        match *self {
            Prefix::Unqualified(..) |
            Prefix::Nested(..) |
            Prefix::TemplateParam(..) => true,
            _ => false,
        }
    }
}

impl PrefixHandle {
    fn is_template_prefix(&self, subs: &SubstitutionTable) -> bool {
        match *self {
            PrefixHandle::BackReference(idx) => {
                if let Some(&Substitutable::Prefix(ref p)) = subs.get(idx) {
                    p.is_template_prefix()
                } else {
                    false
                }
            }
            _ => false,
        }
    }
}

impl Demangle for Prefix {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            Prefix::Unqualified(ref unqualified) => unqualified.demangle(ctx, stack),
            Prefix::Nested(ref prefix, ref unqualified) => {
                try!(prefix.demangle(ctx, stack));
                try!(write!(ctx, "::"));
                unqualified.demangle(ctx, stack)
            }
            Prefix::Template(ref prefix, ref args) => {
                try!(prefix.demangle(ctx, stack));
                args.demangle(ctx, stack)
            }
            Prefix::TemplateParam(ref param) => param.demangle(ctx, stack),
            Prefix::Decltype(ref dt) => dt.demangle(ctx, stack),
            Prefix::DataMember(ref prefix, ref member) => {
                try!(prefix.demangle(ctx, stack));
                member.demangle(ctx, stack)
            }
        }
    }
}


/// The `<unqualified-name>` production.
///
/// ```text
/// <unqualified-name> ::= <operator-name>
///                    ::= <ctor-dtor-name>
///                    ::= <source-name>
///                    ::= <unnamed-type-name>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum UnqualifiedName {
    /// An operator name.
    Operator(OperatorName),
    /// A constructor or destructor name.
    CtorDtor(CtorDtorName),
    /// A source name.
    Source(SourceName),
    /// A generated name for an unnamed type.
    UnnamedType(UnnamedTypeName),
}

impl Parse for UnqualifiedName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnqualifiedName, IndexStr<'b>)> {
        log_parse!("UnqualifiedName", input);

        if let Ok((op, tail)) = OperatorName::parse(subs, input) {
            return Ok((UnqualifiedName::Operator(op), tail));
        }

        if let Ok((ctor_dtor, tail)) = CtorDtorName::parse(subs, input) {
            return Ok((UnqualifiedName::CtorDtor(ctor_dtor), tail));
        }

        if let Ok((source, tail)) = SourceName::parse(subs, input) {
            return Ok((UnqualifiedName::Source(source), tail));
        }

        UnnamedTypeName::parse(subs, input)
            .map(|(unnamed, tail)| (UnqualifiedName::UnnamedType(unnamed), tail))
    }
}

impl StartsWith for UnqualifiedName {
    #[inline]
    fn starts_with(byte: u8) -> bool {
        OperatorName::starts_with(byte) || CtorDtorName::starts_with(byte) ||
        SourceName::starts_with(byte) || UnnamedTypeName::starts_with(byte)
    }
}

impl Demangle for UnqualifiedName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            UnqualifiedName::Operator(ref op_name) => {
                try!(write!(ctx, "operator"));
                op_name.demangle(ctx, stack)
            }
            UnqualifiedName::CtorDtor(ref ctor_dtor) => ctor_dtor.demangle(ctx, stack),
            UnqualifiedName::Source(ref name) => name.demangle(ctx, stack),
            UnqualifiedName::UnnamedType(ref unnamed) => unnamed.demangle(ctx, stack),
        }
    }
}

/// The `<source-name>` non-terminal.
///
/// ```text
/// <source-name> ::= <positive length number> <identifier>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SourceName(Identifier);

impl Parse for SourceName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(SourceName, IndexStr<'b>)> {
        log_parse!("SourceName", input);

        let (source_name_len, input) = try!(parse_number(10, false, input));
        debug_assert!(source_name_len >= 0);
        if source_name_len == 0 {
            return Err(error::Error::UnexpectedText);
        }

        let (head, tail) = match input.try_split_at(source_name_len as _) {
            Some((head, tail)) => (head, tail),
            None => return Err(error::Error::UnexpectedEnd),
        };

        let (identifier, empty) = try!(Identifier::parse(subs, head));
        if !empty.is_empty() {
            return Err(error::Error::UnexpectedText);
        }

        let source_name = SourceName(identifier);
        Ok((source_name, tail))
    }
}

impl StartsWith for SourceName {
    #[inline]
    fn starts_with(byte: u8) -> bool {
        byte == b'0' || (b'0' <= byte && byte <= b'9')
    }
}

impl Demangle for SourceName {
    #[inline]
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        self.0.demangle(ctx, stack)
    }
}

/// The `<identifier>` pseudo-terminal.
///
/// ```text
/// <identifier> ::= <unqualified source code identifier>
/// ```
///
/// > `<identifier>` is a pseudo-terminal representing the characters in the
/// > unqualified identifier for the entity in the source code. This ABI does not
/// > yet specify a mangling for identifiers containing characters outside of
/// > `_A-Za-z0-9`.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Identifier {
    start: usize,
    end: usize,
}

impl Parse for Identifier {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Identifier, IndexStr<'b>)> {
        log_parse!("Identifier", input);

        if input.is_empty() {
            return Err(error::Error::UnexpectedEnd);
        }

        let end = input.as_ref()
            .iter()
            .map(|&c| c as char)
            .take_while(|&c| c == '_' || c.is_digit(36))
            .count();

        if end == 0 {
            return Err(error::Error::UnexpectedText);
        }

        let tail = input.range_from(end..);

        let identifier = Identifier {
            start: input.index(),
            end: tail.index(),
        };

        Ok((identifier, tail))
    }
}

impl Demangle for Identifier {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   _: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        let ident = &ctx.input[self.start..self.end];
        try!(write!(ctx, "{}", String::from_utf8_lossy(ident)));
        Ok(())
    }
}

/// The `<number>` production.
///
/// ```text
/// <number> ::= [n] <non-negative decimal integer>
/// ```
type Number = isize;

impl Parse for Number {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(isize, IndexStr<'b>)> {
        log_parse!("Number", input);

        parse_number(10, true, input)
    }
}

/// A <seq-id> production encoding a base-36 positive number.
///
/// ```text
/// <seq-id> ::= <0-9A-Z>+
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SeqId(usize);

impl Parse for SeqId {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(SeqId, IndexStr<'b>)> {
        log_parse!("SeqId", input);

        parse_number(36, false, input).map(|(num, tail)| (SeqId(num as _), tail))
    }
}

// TODO: support the rest of <operator-name>:
//
// ::= cv <type>               # (cast)
// ::= li <source-name>        # operator ""
// ::= v <digit> <source-name> # vendor extended operator
define_vocabulary! {
    /// The `<operator-name>` production.
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub enum OperatorName {
        New              (b"nw",  "new"),
        NewArray         (b"na",  "new[]"),
        Delete           (b"dl",  "delete"),
        DeleteArray      (b"da",  "delete[]"),
        UnaryPlus        (b"ps",  "+"), // unary
        Neg              (b"ng",  "-"), // unary
        AddressOf        (b"ad",  "&"), // unary
        Deref            (b"de",  "*"), // unary
        BitNot           (b"co",  "~"),
        Add              (b"pl",  "+"),
        Sub              (b"mi",  "-"),
        Mul              (b"ml",  "*"),
        Div              (b"dv",  "/"),
        Rem              (b"rm",  "%"),
        BitAnd           (b"an",  "&"),
        BitOr            (b"or",  "|"),
        BitXor           (b"eo",  "^"),
        Assign           (b"aS",  "="),
        AddAssign        (b"pL",  "+="),
        SubAssign        (b"mI",  "-="),
        MulAssign        (b"mL",  "*="),
        DivAssign        (b"dV",  "/="),
        RemAssign        (b"rM",  "%="),
        BitAndAssign     (b"aN",  "&="),
        BitOrAssign      (b"oR",  "|="),
        BitXorAssign     (b"eO",  "^="),
        Shl              (b"ls",  "<<"),
        Shr              (b"rs",  ">>"),
        ShlAssign        (b"lS",  "<<="),
        ShrAssign        (b"rS",  ">>="),
        Eq               (b"eq",  "=="),
        Ne               (b"ne",  "!="),
        Less             (b"lt",  "<"),
        Greater          (b"gt",  ">"),
        LessEq           (b"le",  "<="),
        GreaterEq        (b"ge",  ">="),
        Not              (b"nt",  "!"),
        LogicalAnd       (b"aa",  "&&"),
        LogicalOr        (b"oo",  "||"),
        PostInc          (b"pp",  "++"), // (postfix in <expression> context)
        PostDec          (b"mm",  "--"), // (postfix in <expression> context)
        Comma            (b"cm",  ","),
        DerefMemberPtr   (b"pm",  "->*"),
        DerefMember      (b"pt",  "->"),
        Call             (b"cl",  "()"),
        Index            (b"ix",  "[]"),
        Question         (b"qu",  "?:")
    }
}

/// The `<call-offset>` production.
///
/// ```text
/// <call-offset> ::= h <nv-offset> _
///               ::= v <v-offset> _
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum CallOffset {
    /// A non-virtual offset.
    NonVirtual(NvOffset),
    /// A virtual offset.
    Virtual(VOffset),
}

impl Parse for CallOffset {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(CallOffset, IndexStr<'b>)> {
        log_parse!("CallOffset", input);

        if input.is_empty() {
            return Err(error::Error::UnexpectedEnd);
        }

        if let Ok(tail) = consume(b"h", input) {
            let (offset, tail) = try!(NvOffset::parse(subs, tail));
            let tail = try!(consume(b"_", tail));
            return Ok((CallOffset::NonVirtual(offset), tail));
        }

        if let Ok(tail) = consume(b"v", input) {
            let (offset, tail) = try!(VOffset::parse(subs, tail));
            let tail = try!(consume(b"_", tail));
            return Ok((CallOffset::Virtual(offset), tail));
        }

        Err(error::Error::UnexpectedText)
    }
}

impl Demangle for CallOffset {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   _: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            CallOffset::NonVirtual(NvOffset(offset)) => {
                try!(write!(ctx, "{{offset({})}}", offset));
            }
            CallOffset::Virtual(VOffset(vbase, vcall)) => {
                try!(write!(ctx, "{{virtual offset({}, {})}}", vbase, vcall));
            }
        }
        Ok(())
    }
}

/// A non-virtual offset, as described by the <nv-offset> production.
///
/// ```text
/// <nv-offset> ::= <offset number>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct NvOffset(isize);

impl Parse for NvOffset {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(NvOffset, IndexStr<'b>)> {
        log_parse!("NvOffset", input);

        Number::parse(subs, input).map(|(num, tail)| (NvOffset(num), tail))
    }
}

/// A virtual offset, as described by the <v-offset> production.
///
/// ```text
/// <v-offset> ::= <offset number> _ <virtual offset number>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct VOffset(isize, isize);

impl Parse for VOffset {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(VOffset, IndexStr<'b>)> {
        log_parse!("VOffset", input);

        let (offset, tail) = try!(Number::parse(subs, input));
        let tail = try!(consume(b"_", tail));
        let (virtual_offset, tail) = try!(Number::parse(subs, tail));
        Ok((VOffset(offset, virtual_offset), tail))
    }
}

define_vocabulary! {
    /// The `<ctor-dtor-name>` production.
    ///
    /// ```text
    /// <ctor-dtor-name> ::= C1  # complete object constructor
    ///                  ::= C2  # base object constructor
    ///                  ::= C3  # complete object allocating constructor
    ///                  ::= D0  # deleting destructor
    ///                  ::= D1  # complete object destructor
    ///                  ::= D2  # base object destructor
    /// ```
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub enum CtorDtorName {
        CompleteConstructor             (b"C1", "complete object constructor"),
        BaseConstructor                 (b"C2", "base object constructor"),
        CompleteAllocatingConstructor   (b"C3", "complete object allocating constructor"),
        DeletingDestructor              (b"D0", "deleting destructor"),
        CompleteDestructor              (b"D1", "complete object destructor"),
        BaseDestructor                  (b"D2", "base object destructor")
    }
}

/// The `<type>` production.
///
/// ```text
/// <type> ::= <builtin-type>
///        ::= <function-type>
///        ::= <class-enum-type>
///        ::= <array-type>
///        ::= <pointer-to-member-type>
///        ::= <template-param>
///        ::= <template-template-param> <template-args>
///        ::= <decltype>
///        ::= <CV-qualifiers> <type>
///        ::= P <type>                                 # pointer-to
///        ::= R <type>                                 # reference-to
///        ::= O <type>                                 # rvalue reference-to (C++0x)
///        ::= C <type>                                 # complex pair (C 2000)
///        ::= G <type>                                 # imaginary (C 2000)
///        ::= U <source-name> [<template-args>] <type> # vendor extended type qualifier
///        ::= Dp <type>                                # pack expansion (C++0x)
///        ::= <substitution>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Type {
    /// A function type.
    Function(FunctionType),

    /// A class, union, or enum type.
    ClassEnum(ClassEnumType),

    /// An array type.
    Array(ArrayType),

    /// A pointer-to-member type.
    PointerToMember(PointerToMemberType),

    /// A named template parameter type.
    TemplateParam(TemplateParam),

    /// A template template type.
    TemplateTemplate(TemplateTemplateParamHandle, TemplateArgs),

    /// A decltype.
    Decltype(Decltype),

    /// A const-, restrict-, and/or volatile-qualified type.
    Qualified(CvQualifiers, TypeHandle),

    /// A pointer to a type.
    PointerTo(TypeHandle),

    /// An lvalue reference to a type.
    LvalueRef(TypeHandle),

    /// An rvalue reference to a type.
    RvalueRef(TypeHandle),

    /// A complex pair of the given type.
    Complex(TypeHandle),

    /// An imaginary of the given type.
    Imaginary(TypeHandle),

    /// A vendor extended type qualifier.
    VendorExtension(SourceName, Option<TemplateArgs>, TypeHandle),

    /// A pack expansion.
    PackExpansion(TypeHandle),
}

define_handle! {
    /// A reference to a parsed `Type` production.
    pub enum TypeHandle {
        /// A builtin type.
        extra Builtin(BuiltinType),
    }
}

impl TypeHandle {
    fn is_void(&self) -> bool {
        match *self {
            TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Void)) => true,
            _ => false,
        }
    }
}

impl Parse for TypeHandle {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(TypeHandle, IndexStr<'b>)> {
        log_parse!("TypeHandle", input);

        if let Ok((sub, tail)) = Substitution::parse(subs, input) {
            // If we see an 'I', then this is actually a substitution for a
            // <template-template-param>, and the template args are what
            // follows. Throw away what we just parsed, and re-parse it in
            // `TemplateTemplateParamHandle::parse` for now, but it would be
            // nice not to duplicate work we've already done.
            if tail.peek() != Some(b'I') {
                match sub {
                    Substitution::WellKnown(component) => {
                        return Ok((TypeHandle::WellKnown(component), tail));
                    }
                    Substitution::BackReference(idx) => {
                        // TODO: should this check if the back reference actually points
                        // to a <type>?
                        return Ok((TypeHandle::BackReference(idx), tail));
                    }
                }
            }
        }

        /// Insert the given type into the substitution table, and return a
        /// handle referencing the index in the table where it ended up.
        fn insert_and_return_handle<'a, 'b>(ty: Type,
                                            subs: &'a mut SubstitutionTable,
                                            tail: IndexStr<'b>)
                                            -> Result<(TypeHandle, IndexStr<'b>)> {
            let ty = Substitutable::Type(ty);
            let idx = subs.insert(ty);
            let handle = TypeHandle::BackReference(idx);
            Ok((handle, tail))
        }

        if let Ok((builtin, tail)) = BuiltinType::parse(subs, input) {
            // Builtin types are one of two exceptions that do not end up in the
            // substitutions table.
            let handle = TypeHandle::Builtin(builtin);
            return Ok((handle, tail));
        }

        if let Ok((funty, tail)) = FunctionType::parse(subs, input) {
            let ty = Type::Function(funty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok((ty, tail)) = ClassEnumType::parse(subs, input) {
            let ty = Type::ClassEnum(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok((ty, tail)) = ArrayType::parse(subs, input) {
            let ty = Type::Array(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok((ty, tail)) = PointerToMemberType::parse(subs, input) {
            let ty = Type::PointerToMember(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok((param, tail)) = TemplateParam::parse(subs, input) {
            // Same situation as with `Substitution::parse` at the top of this
            // function: this is actually a <template-template-param> and
            // <template-args>.
            if tail.peek() != Some(b'I') {
                let ty = Type::TemplateParam(param);
                return insert_and_return_handle(ty, subs, tail);
            }
        }

        if let Ok((ttp, tail)) = TemplateTemplateParamHandle::parse(subs, input) {
            let (args, tail) = try!(TemplateArgs::parse(subs, tail));
            let ty = Type::TemplateTemplate(ttp, args);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok((param, tail)) = Decltype::parse(subs, input) {
            let ty = Type::Decltype(param);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok((qualifiers, tail)) = CvQualifiers::parse(subs, input) {
            // CvQualifiers can parse successfully without consuming any input,
            // but we don't want to recurse unless we know we did consume some
            // input, lest we go into an infinite loop and blow the stack.
            if tail.len() < input.len() {
                let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                let ty = Type::Qualified(qualifiers, ty);
                return insert_and_return_handle(ty, subs, tail);
            }
        }

        if let Ok(tail) = consume(b"P", input) {
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            let ty = Type::PointerTo(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok(tail) = consume(b"R", input) {
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            let ty = Type::LvalueRef(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok(tail) = consume(b"O", input) {
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            let ty = Type::RvalueRef(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok(tail) = consume(b"C", input) {
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            let ty = Type::Complex(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok(tail) = consume(b"G", input) {
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            let ty = Type::Imaginary(ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        if let Ok(tail) = consume(b"U", input) {
            let (name, tail) = try!(SourceName::parse(subs, tail));
            let (args, tail) = if let Ok((args, tail)) = TemplateArgs::parse(subs,
                                                                             tail) {
                (Some(args), tail)
            } else {
                (None, tail)
            };
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            let ty = Type::VendorExtension(name, args, ty);
            return insert_and_return_handle(ty, subs, tail);
        }

        let tail = try!(consume(b"Dp", input));
        let (ty, tail) = try!(TypeHandle::parse(subs, tail));
        let ty = Type::PackExpansion(ty);
        insert_and_return_handle(ty, subs, tail)
    }
}

impl DemangleWithInner for Type {
    fn demangle_with_inner<D, W>(&self,
                                 inner: Option<&D>,
                                 ctx: &mut DemangleContext<W>,
                                 stack: Option<ArgStack>)
                                 -> io::Result<()>
        where D: ?Sized + Demangle,
              W: io::Write
    {
        match *self {
            Type::Function(ref func_ty) => func_ty.demangle(ctx, stack),
            Type::ClassEnum(ref cls_enum_ty) => cls_enum_ty.demangle(ctx, stack),
            Type::Array(ref array_ty) => array_ty.demangle(ctx, stack),
            Type::PointerToMember(ref ptm) => ptm.demangle(ctx, stack),
            Type::TemplateParam(ref param) => param.demangle(ctx, stack),
            Type::TemplateTemplate(ref tt_param, ref args) => {
                try!(tt_param.demangle(ctx, stack));
                args.demangle(ctx, stack)
            }
            Type::Decltype(ref dt) => dt.demangle(ctx, stack),
            Type::Qualified(ref quals, ref ty) => {
                if let Some(ty @ &Type::PointerTo(_)) = ctx.subs.get_type(ty) {
                    ty.demangle_with_inner(Some(quals), ctx, stack)
                } else {
                    try!(ty.demangle(ctx, stack));
                    try!(write!(ctx, " "));
                    quals.demangle(ctx, stack)
                }
            }
            Type::PointerTo(ref ty) => {
                fn demangle_pointer<D, W>(ty: &TypeHandle,
                                          inner: &D,
                                          ctx: &mut DemangleContext<W>,
                                          stack: Option<ArgStack>)
                                          -> io::Result<()>
                    where D: ?Sized + Demangle,
                          W: io::Write
                {
                    match ctx.subs.get_type(ty) {
                        Some(&Type::Array(ref array_type)) => {
                            array_type.demangle_with_inner(Some(inner), ctx, stack)
                        }
                        Some(&Type::Function(ref func)) => {
                            func.demangle_with_inner(Some(inner), ctx, stack)
                        }
                        _ => {
                            try!(ty.demangle(ctx, stack));
                            try!(inner.demangle(ctx, stack));
                            Ok(())
                        }
                    }
                }
                match inner {
                    Some(inner) => {
                        let concat = Concat("* ", inner);
                        demangle_pointer(ty, &concat, ctx, stack)
                    }
                    None => {
                        demangle_pointer(ty, "*", ctx, stack)
                    }
                }
            }
            Type::LvalueRef(ref ty) => {
                match ctx.subs.get_type(ty) {
                    Some(&Type::Array(ref array_type)) => {
                        array_type.demangle_with_inner(Some("&"), ctx, stack)
                    }
                    Some(&Type::Function(ref func)) => {
                        func.demangle_with_inner(Some("&"), ctx, stack)
                    }
                    _ => {
                        try!(ty.demangle(ctx, stack));
                        try!(write!(ctx, "&"));
                        Ok(())
                    }
                }
            }
            Type::RvalueRef(ref ty) => {
                match ctx.subs.get_type(ty) {
                    Some(&Type::Array(ref array_type)) => {
                        array_type.demangle_with_inner(Some("&&"), ctx, stack)
                    }
                    Some(&Type::Function(ref func)) => {
                        func.demangle_with_inner(Some("&&"), ctx, stack)
                    }
                    _ => {
                        try!(ty.demangle(ctx, stack));
                        try!(write!(ctx, "&&"));
                        Ok(())
                    }
                }
            }
            Type::Complex(ref ty) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, " _Complex"));
                Ok(())
            }
            Type::Imaginary(ref ty) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, " _Imaginary"));
                Ok(())
            }
            Type::VendorExtension(ref name, ref template_args, ref ty) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, " "));
                try!(name.demangle(ctx, stack));
                if let Some(ref args) = *template_args {
                    try!(args.demangle(ctx, stack));
                }
                Ok(())
            }
            Type::PackExpansion(ref ty) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, "..."));
                Ok(())
            }
        }
    }
}

/// The `<CV-qualifiers>` production.
///
/// ```text
/// <CV-qualifiers> ::= [r] [V] [K]   # restrict (C99), volatile, const
/// ```
#[derive(Clone, Debug, Default, Hash, PartialEq, Eq)]
pub struct CvQualifiers {
    /// Is this `restrict` qualified?
    pub restrict: bool,
    /// Is this `volatile` qualified?
    pub volatile: bool,
    /// Is this `const` qualified?
    pub const_: bool,
}

impl Parse for CvQualifiers {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(CvQualifiers, IndexStr<'b>)> {
        log_parse!("CvQualifiers", input);

        let (restrict, tail) = if let Ok(tail) = consume(b"r", input) {
            (true, tail)
        } else {
            (false, input)
        };

        let (volatile, tail) = if let Ok(tail) = consume(b"V", tail) {
            (true, tail)
        } else {
            (false, tail)
        };

        let (const_, tail) = if let Ok(tail) = consume(b"K", tail) {
            (true, tail)
        } else {
            (false, tail)
        };

        let qualifiers = CvQualifiers {
            restrict: restrict,
            volatile: volatile,
            const_: const_,
        };

        Ok((qualifiers, tail))
    }
}

impl Demangle for CvQualifiers {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   _: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        if self.const_ {
            try!(ctx.ensure_space());
            try!(write!(ctx, "const"));
        }

        if self.volatile {
            try!(ctx.ensure_space());
            try!(write!(ctx, "volatile"));
        }

        if self.restrict {
            try!(ctx.ensure_space());
            try!(write!(ctx, "restrict"));
        }

        Ok(())
    }
}

define_vocabulary! {
    /// A <ref-qualifier> production.
    ///
    /// ```text
    /// <ref-qualifier> ::= R   # & ref-qualifier
    ///                 ::= O   # && ref-qualifier
    /// ```
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub enum RefQualifier {
        LValueRef(b"R", "&"),
        RValueRef(b"O", "&&")
    }
}

define_vocabulary! {
    /// A one of the standard variants of the <builtin-type> production.
    ///
    /// ```text
    /// <builtin-type> ::= v  # void
    ///                ::= w  # wchar_t
    ///                ::= b  # bool
    ///                ::= c  # char
    ///                ::= a  # signed char
    ///                ::= h  # unsigned char
    ///                ::= s  # short
    ///                ::= t  # unsigned short
    ///                ::= i  # int
    ///                ::= j  # unsigned int
    ///                ::= l  # long
    ///                ::= m  # unsigned long
    ///                ::= x  # long long, __int64
    ///                ::= y  # unsigned long long, __int64
    ///                ::= n  # __int128
    ///                ::= o  # unsigned __int128
    ///                ::= f  # float
    ///                ::= d  # double
    ///                ::= e  # long double, __float80
    ///                ::= g  # __float128
    ///                ::= z  # ellipsis
    ///                ::= Dd # IEEE 754r decimal floating point (64 bits)
    ///                ::= De # IEEE 754r decimal floating point (128 bits)
    ///                ::= Df # IEEE 754r decimal floating point (32 bits)
    ///                ::= Dh # IEEE 754r half-precision floating point (16 bits)
    ///                ::= Di # char32_t
    ///                ::= Ds # char16_t
    ///                ::= Da # auto
    ///                ::= Dc # decltype(auto)
    ///                ::= Dn # std::nullptr_t (i.e., decltype(nullptr))
    /// ```
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub enum StandardBuiltinType {
        Void             (b"v",  "void"),
        Wchar            (b"w",  "wchar_t"),
        Bool             (b"b",  "bool"),
        Char             (b"c",  "char"),
        SignedChar       (b"a",  "signed char"),
        UnsignedChar     (b"h",  "unsigned char"),
        Short            (b"s",  "short"),
        UnsignedShort    (b"t",  "unsigned short"),
        Int              (b"i",  "int"),
        UnsignedInt      (b"j",  "unsigned int"),
        Long             (b"l",  "long"),
        UnsignedLong     (b"m",  "unsigned long"),
        LongLong         (b"x",  "long long"),
        UnsignedLongLong (b"y",  "unsigned long long"),
        Int128           (b"n",  "__int128"),
        Uint128          (b"o",  "unsigned __int128"),
        Float            (b"f",  "float"),
        Double           (b"d",  "double"),
        LongDouble       (b"e",  "long double"),
        Float128         (b"g",  "__float128"),
        Ellipsis         (b"z",  "ellipsis"),
        DecimalFloat64   (b"Dd", "_Decimal64"),
        DecimalFloat128  (b"De", "_Decimal128"),
        DecimalFloat32   (b"Df", "_Decimal32"),
        DecimalFloat16   (b"Dh", "_Decimal16"),
        Char32           (b"Di", "char32_t"),
        Char16           (b"Ds", "char16_t"),
        Auto             (b"Da", "auto"),
        Decltype         (b"Dc", "decltype(auto)"),
        Nullptr          (b"Dn", "std::nullptr_t")
    }
}

/// The `<builtin-type>` production.
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum BuiltinType {
    /// A standards compliant builtin type.
    Standard(StandardBuiltinType),

    /// A non-standard, vendor extension type.
    ///
    /// ```text
    /// <builtin-type> ::= u <source-name>   # vendor extended type
    /// ```
    Extension(SourceName),
}

impl Parse for BuiltinType {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(BuiltinType, IndexStr<'b>)> {
        log_parse!("BuiltinType", input);

        if let Ok((ty, tail)) = StandardBuiltinType::parse(subs, input) {
            return Ok((BuiltinType::Standard(ty), tail));
        }

        let tail = try!(consume(b"u", input));
        let (name, tail) = try!(SourceName::parse(subs, tail));
        Ok((BuiltinType::Extension(name), tail))
    }
}

impl Demangle for BuiltinType {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            BuiltinType::Standard(ref ty) => ty.demangle(ctx, stack),
            BuiltinType::Extension(ref name) => name.demangle(ctx, stack),
        }
    }
}

/// The `<function-type>` production.
///
/// ```text
/// <function-type> ::= [<CV-qualifiers>] [Dx] F [Y] <bare-function-type> [<ref-qualifier>] E
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FunctionType {
    cv_qualifiers: CvQualifiers,
    transaction_safe: bool,
    extern_c: bool,
    bare: BareFunctionType,
    ref_qualifier: Option<RefQualifier>,
}

impl Parse for FunctionType {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(FunctionType, IndexStr<'b>)> {
        log_parse!("FunctionType", input);

        let (cv_qualifiers, tail) = if let Ok((cv_qualifiers, tail)) =
            CvQualifiers::parse(subs, input) {
            (cv_qualifiers, tail)
        } else {
            (Default::default(), input)
        };

        let (transaction_safe, tail) = if let Ok(tail) = consume(b"Dx", tail) {
            (true, tail)
        } else {
            (false, tail)
        };

        let tail = try!(consume(b"F", tail));

        let (extern_c, tail) = if let Ok(tail) = consume(b"Y", tail) {
            (true, tail)
        } else {
            (false, tail)
        };

        let (bare, tail) = try!(BareFunctionType::parse(subs, tail));

        let (ref_qualifier, tail) = if let Ok((ref_qualifier, tail)) =
            RefQualifier::parse(subs, tail) {
            (Some(ref_qualifier), tail)
        } else {
            (None, tail)
        };

        let tail = try!(consume(b"E", tail));

        let func_ty = FunctionType {
            cv_qualifiers: cv_qualifiers,
            transaction_safe: transaction_safe,
            extern_c: extern_c,
            bare: bare,
            ref_qualifier: ref_qualifier,
        };
        Ok((func_ty, tail))
    }
}

impl DemangleWithInner for FunctionType {
    fn demangle_with_inner<D, W>(&self,
                                 inner: Option<&D>,
                                 ctx: &mut DemangleContext<W>,
                                 stack: Option<ArgStack>)
                                 -> io::Result<()>
        where D: ?Sized + Demangle,
              W: io::Write
    {
        // TODO: transactions safety?
        // TODO: extern C?
        try!(self.bare.demangle_with_inner(inner, ctx, stack));
        try!(self.cv_qualifiers.demangle(ctx, stack));
        // TODO: ref_qualifier?
        Ok(())
    }
}

/// The `<bare-function-type>` production.
///
/// ```text
/// <bare-function-type> ::= <signature type>+
///      # types are possible return type, then parameter types
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct BareFunctionType(Vec<TypeHandle>);

impl BareFunctionType {
    fn ret(&self) -> &TypeHandle {
        &self.0[0]
    }

    fn args(&self) -> &[TypeHandle] {
        &self.0[1..]
    }
}

impl Parse for BareFunctionType {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(BareFunctionType, IndexStr<'b>)> {
        log_parse!("BareFunctionType", input);

        let (types, tail) = try!(one_or_more::<TypeHandle>(subs, input));
        Ok((BareFunctionType(types), tail))
    }
}

impl DemangleWithInner for BareFunctionType {
    fn demangle_with_inner<D, W>(&self,
                                 inner: Option<&D>,
                                 ctx: &mut DemangleContext<W>,
                                 stack: Option<ArgStack>)
                                 -> io::Result<()>
        where D: ?Sized + Demangle,
              W: io::Write
    {
        try!(self.ret().demangle(ctx, stack));
        try!(ctx.ensure_space());

        if let Some(inner) = inner {
            try!(write!(ctx, "("));
            try!(inner.demangle(ctx, stack));
            try!(write!(ctx, ")"));
        }

        let args = FunctionArgList(self.args());
        args.demangle(ctx, stack)
    }
}

/// The `<decltype>` production.
///
/// ```text
/// <decltype> ::= Dt <expression> E
///            ::= DT <expression> E
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Decltype {
    /// A `decltype` of an id-expression or class member access (C++0x).
    IdExpression(Expression),

    /// A `decltype` of an expression (C++0x).
    Expression(Expression),
}

impl Parse for Decltype {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Decltype, IndexStr<'b>)> {
        log_parse!("Decltype", input);

        let tail = try!(consume(b"D", input));

        if let Ok(tail) = consume(b"t", tail) {
            let (expr, tail) = try!(Expression::parse(subs, tail));
            let tail = try!(consume(b"E", tail));
            return Ok((Decltype::IdExpression(expr), tail));
        }

        let tail = try!(consume(b"T", tail));
        let (expr, tail) = try!(Expression::parse(subs, tail));
        let tail = try!(consume(b"E", tail));
        Ok((Decltype::Expression(expr), tail))
    }
}

impl Demangle for Decltype {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            Decltype::Expression(ref expr) |
            Decltype::IdExpression(ref expr) => {
                try!(write!(ctx, "decltype ("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
        }
    }
}

/// The `<class-enum-type>` production.
///
/// ```text
/// <class-enum-type> ::= <name>
///                   ::= Ts <name>
///                   ::= Tu <name>
///                   ::= Te <name>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ClassEnumType {
    /// A non-dependent type name, dependent type name, or dependent
    /// typename-specifier.
    Named(Name),

    /// A dependent elaborated type specifier using 'struct' or 'class'.
    ElaboratedStruct(Name),

    /// A dependent elaborated type specifier using 'union'.
    ElaboratedUnion(Name),

    /// A dependent elaborated type specifier using 'enum'.
    ElaboratedEnum(Name),
}

impl Parse for ClassEnumType {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(ClassEnumType, IndexStr<'b>)> {
        log_parse!("ClassEnumType", input);

        if let Ok((name, tail)) = Name::parse(subs, input) {
            return Ok((ClassEnumType::Named(name), tail));
        }

        let tail = try!(consume(b"T", input));

        if let Ok(tail) = consume(b"s", tail) {
            let (name, tail) = try!(Name::parse(subs, tail));
            return Ok((ClassEnumType::ElaboratedStruct(name), tail));
        }

        if let Ok(tail) = consume(b"u", tail) {
            let (name, tail) = try!(Name::parse(subs, tail));
            return Ok((ClassEnumType::ElaboratedUnion(name), tail));
        }

        let tail = try!(consume(b"e", tail));
        let (name, tail) = try!(Name::parse(subs, tail));
        Ok((ClassEnumType::ElaboratedEnum(name), tail))
    }
}

impl Demangle for ClassEnumType {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            ClassEnumType::Named(ref name) => name.demangle(ctx, stack),
            ClassEnumType::ElaboratedStruct(ref name) => {
                try!(write!(ctx, "class "));
                name.demangle(ctx, stack)
            }
            ClassEnumType::ElaboratedUnion(ref name) => {
                try!(write!(ctx, "union "));
                name.demangle(ctx, stack)
            }
            ClassEnumType::ElaboratedEnum(ref name) => {
                try!(write!(ctx, "enum "));
                name.demangle(ctx, stack)
            }
        }
    }
}

/// The `<unnamed-type-name>` production.
///
/// ```text
/// <unnamed-type-name> ::= Ut [ <nonnegative number> ] _
///                     ::= <closure-type-name>
/// ```
///
/// TODO: parse the <closure-type-name> variant
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UnnamedTypeName(Option<usize>);

impl Parse for UnnamedTypeName {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnnamedTypeName, IndexStr<'b>)> {
        log_parse!("UnnamedTypeName", input);

        let input = try!(consume(b"Ut", input));
        let (number, input) = match parse_number(10, false, input) {
            Ok((number, input)) => (Some(number as _), input),
            Err(_) => (None, input),
        };
        let input = try!(consume(b"_", input));
        Ok((UnnamedTypeName(number), input))
    }
}

impl StartsWith for UnnamedTypeName {
    #[inline]
    fn starts_with(byte: u8) -> bool {
        byte == b'U'
    }
}

impl Demangle for UnnamedTypeName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   _: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "{{unnamed type {}}}", self.0.map_or(0, |n| n + 1)));
        Ok(())
    }
}

/// The `<array-type>` production.
///
/// ```text
/// <array-type> ::= A <positive dimension number> _ <element type>
///              ::= A [<dimension expression>] _ <element type>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ArrayType {
    /// An array with a number-literal dimension.
    DimensionNumber(usize, TypeHandle),

    /// An array with an expression for its dimension.
    DimensionExpression(Expression, TypeHandle),

    /// An array with no dimension.
    NoDimension(TypeHandle),
}

impl Parse for ArrayType {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(ArrayType, IndexStr<'b>)> {
        log_parse!("ArrayType", input);

        let tail = try!(consume(b"A", input));

        if let Ok((num, tail)) = parse_number(10, false, tail) {
            debug_assert!(num >= 0);
            let tail = try!(consume(b"_", tail));
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            return Ok((ArrayType::DimensionNumber(num as _, ty), tail));
        }

        if let Ok((expr, tail)) = Expression::parse(subs, tail) {
            let tail = try!(consume(b"_", tail));
            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
            return Ok((ArrayType::DimensionExpression(expr, ty), tail));
        }

        let tail = try!(consume(b"_", tail));
        let (ty, tail) = try!(TypeHandle::parse(subs, tail));
        Ok((ArrayType::NoDimension(ty), tail))
    }
}

impl DemangleWithInner for ArrayType {
    fn demangle_with_inner<D, W>(&self,
                              inner: Option<&D>,
                              ctx: &mut DemangleContext<W>,
                              stack: Option<ArgStack>)
                              -> io::Result<()>
        where D: ?Sized + Demangle,
              W: io::Write
    {
        match *self {
            ArrayType::DimensionNumber(n, ref ty) => {
                try!(ty.demangle(ctx, stack));
                if let Some(inner) = inner {
                    try!(write!(ctx, " ("));
                    try!(inner.demangle(ctx, stack));
                    try!(write!(ctx, ") [{}]", n));
                } else {
                    try!(write!(ctx, " [{}]", n));
                }
            }
            ArrayType::DimensionExpression(ref expr, ref ty) => {
                try!(ty.demangle(ctx, stack));
                if let Some(inner) = inner {
                    try!(write!(ctx, " ("));
                    try!(inner.demangle(ctx, stack));
                    try!(write!(ctx, ") ["));
                } else {
                    try!(write!(ctx, " ["));
                }
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, "]"));
            }
            ArrayType::NoDimension(ref ty) => {
                try!(ty.demangle(ctx, stack));
                if let Some(inner) = inner {
                    try!(write!(ctx, " ("));
                    try!(inner.demangle(ctx, stack));
                    try!(write!(ctx, ") []"));
                } else {
                    try!(write!(ctx, " []"));
                }
            }
        }
        Ok(())
    }
}

/// The `<pointer-to-member-type>` production.
///
/// ```text
/// <pointer-to-member-type> ::= M <class type> <member type>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct PointerToMemberType(TypeHandle, TypeHandle);

impl Parse for PointerToMemberType {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(PointerToMemberType, IndexStr<'b>)> {
        log_parse!("PointerToMemberType", input);

        let tail = try!(consume(b"M", input));
        let (ty1, tail) = try!(TypeHandle::parse(subs, tail));
        let (ty2, tail) = try!(TypeHandle::parse(subs, tail));
        Ok((PointerToMemberType(ty1, ty2), tail))
    }
}

impl Demangle for PointerToMemberType {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        if let Some(&Type::Function(ref func)) = ctx.subs.get_type(&self.1) {
            let ptm = Concat(&self.0, "::*");
            func.demangle_with_inner(Some(&ptm), ctx, stack)
        } else {
            try!(self.1.demangle(ctx, stack));
            try!(write!(ctx, " "));
            try!(self.0.demangle(ctx, stack));
            try!(write!(ctx, "::*"));
            Ok(())
        }
    }
}

/// The `<template-param>` production.
///
/// ```text
/// <template-param> ::= T_ # first template parameter
///                  ::= T <parameter-2 non-negative number> _
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TemplateParam(usize);

impl Parse for TemplateParam {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(TemplateParam, IndexStr<'b>)> {
        log_parse!("TemplateParam", input);

        let input = try!(consume(b"T", input));
        let (number, input) = match parse_number(10, false, input) {
            Ok((number, input)) => ((number + 1) as _, input),
            Err(_) => (0, input),
        };
        let input = try!(consume(b"_", input));
        Ok((TemplateParam(number), input))
    }
}

impl Demangle for TemplateParam {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        let arg = try!(stack.get_template_arg(self.0)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.description())));
        arg.demangle(ctx, stack)
    }
}

/// The `<template-template-param>` production.
///
/// ```text
/// <template-template-param> ::= <template-param>
///                           ::= <substitution>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TemplateTemplateParam(TemplateParam);

define_handle! {
    /// A reference to a parsed `TemplateTemplateParam`.
    pub enum TemplateTemplateParamHandle
}

impl Parse for TemplateTemplateParamHandle {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(TemplateTemplateParamHandle, IndexStr<'b>)> {
        log_parse!("TemplateTemplateParamHandle", input);


        if let Ok((sub, tail)) = Substitution::parse(subs, input) {
            match sub {
                Substitution::WellKnown(component) => {
                    return Ok((TemplateTemplateParamHandle::WellKnown(component), tail));
                }
                Substitution::BackReference(idx) => {
                    // TODO: should this check if the thing at idx is a
                    // template-template-param? There could otherwise be ambiguity
                    // with <type>'s <substitution> form...
                    return Ok((TemplateTemplateParamHandle::BackReference(idx), tail));
                }
            }
        }

        let (param, tail) = try!(TemplateParam::parse(subs, input));
        let ttp = TemplateTemplateParam(param);
        let ttp = Substitutable::TemplateTemplateParam(ttp);
        let idx = subs.insert(ttp);
        let handle = TemplateTemplateParamHandle::BackReference(idx);
        Ok((handle, tail))
    }
}

impl Demangle for TemplateTemplateParam {
    #[inline]
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        self.0.demangle(ctx, stack)
    }
}

/// The <function-param> production.
///
/// ```text
/// <function-param> ::= fp <top-level CV-qualifiers> _
///                          # L == 0, first parameter
///                  ::= fp <top-level CV-qualifiers> <parameter-2 non-negative number> _
///                          # L == 0, second and later parameters
///                  ::= fL <L-1 non-negative number> p <top-level CV-qualifiers> _
///                          # L > 0, first parameter
///                  ::= fL <L-1 non-negative number> p <top-level CV-qualifiers> <parameter-2 non-negative number> _
///                          # L > 0, second and later parameters
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct FunctionParam(usize, CvQualifiers, Option<usize>);

impl Parse for FunctionParam {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(FunctionParam, IndexStr<'b>)> {
        log_parse!("FunctionParam", input);

        let tail = try!(consume(b"f", input));
        if tail.is_empty() {
            return Err(error::Error::UnexpectedEnd);
        }

        let (scope, tail) = if let Ok(tail) = consume(b"L", tail) {
            try!(parse_number(10, false, tail))
        } else {
            (0, tail)
        };

        let tail = try!(consume(b"p", tail));

        let (qualifiers, tail) = try!(CvQualifiers::parse(subs, tail));

        let (param, tail) = if let Ok((num, tail)) = parse_number(10, false, tail) {
            (Some(num as _), tail)
        } else {
            (None, tail)
        };

        let tail = try!(consume(b"_", tail));
        Ok((FunctionParam(scope as _, qualifiers, param), tail))
    }
}

impl Demangle for FunctionParam {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        // TODO: this needs more finesse.
        let ty = try!(stack.get_function_arg(self.0)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.description())));
        ty.demangle(ctx, stack)
    }
}

/// The `<template-args>` production.
///
/// ```text
/// <template-args> ::= I <template-arg>+ E
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct TemplateArgs(Vec<TemplateArg>);

impl Parse for TemplateArgs {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(TemplateArgs, IndexStr<'b>)> {
        log_parse!("TemplateArgs", input);

        let tail = try!(consume(b"I", input));

        let (args, tail) = try!(one_or_more::<TemplateArg>(subs, tail));
        let tail = try!(consume(b"E", tail));
        Ok((TemplateArgs(args), tail))
    }
}

impl Demangle for TemplateArgs {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "<"));
        let mut need_comma = false;
        for arg in &self.0[..] {
            if need_comma {
                try!(write!(ctx, ", "));
            }
            try!(arg.demangle(ctx, stack));
            need_comma = true;
        }
        try!(write!(ctx, ">"));
        Ok(())
    }
}

impl ArgResolver for TemplateArgs {
    fn get_template_arg(&self, idx: usize) -> Result<&TemplateArg> {
        self.0.get(idx).ok_or(error::Error::BadTemplateArgReference)
    }

    fn get_function_arg(&self, _: usize) -> Result<&Type> {
        Err(error::Error::BadFunctionArgReference)
    }
}

/// A <template-arg> production.
///
/// ```text
/// <template-arg> ::= <type>                # type or template
///                ::= X <expression> E      # expression
///                ::= <expr-primary>        # simple expressions
///                ::= J <template-arg>* E   # argument pack
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum TemplateArg {
    /// A type or template.
    Type(TypeHandle),

    /// An expression.
    Expression(Expression),

    /// A simple expression.
    SimpleExpression(ExprPrimary),

    /// An argument pack.
    ArgPack(Vec<TemplateArg>),
}

impl Parse for TemplateArg {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(TemplateArg, IndexStr<'b>)> {
        log_parse!("TemplateArg", input);

        if let Ok(tail) = consume(b"X", input) {
            let (expr, tail) = try!(Expression::parse(subs, tail));
            let tail = try!(consume(b"E", tail));
            return Ok((TemplateArg::Expression(expr), tail));
        }

        if let Ok((expr, tail)) = ExprPrimary::parse(subs, input) {
            return Ok((TemplateArg::SimpleExpression(expr), tail));
        }

        if let Ok((ty, tail)) = TypeHandle::parse(subs, input) {
            return Ok((TemplateArg::Type(ty), tail));
        }

        let tail = try!(consume(b"J", input));
        let (args, tail) = if tail.peek() == Some(b'E') {
            (vec![], tail)
        } else {
            try!(zero_or_more::<TemplateArg>(subs, tail))
        };
        let tail = try!(consume(b"E", tail));
        Ok((TemplateArg::ArgPack(args), tail))
    }
}

impl Demangle for TemplateArg {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            TemplateArg::Type(ref ty) => ty.demangle(ctx, stack),
            TemplateArg::Expression(ref expr) => expr.demangle(ctx, stack),
            TemplateArg::SimpleExpression(ref expr) => expr.demangle(ctx, stack),
            TemplateArg::ArgPack(ref args) => {
                let mut need_comma = false;
                for arg in &args[..] {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(arg.demangle(ctx, stack));
                    need_comma = true;
                }
                Ok(())
            }
        }
    }
}

/// The `<expression>` production.
///
/// ```text
///  <expression> ::= <unary operator-name> <expression>
///               ::= <binary operator-name> <expression> <expression>
///               ::= <ternary operator-name> <expression> <expression> <expression>
///               ::= pp_ <expression>                             # prefix ++
///               ::= mm_ <expression>                             # prefix --
///               ::= cl <expression>+ E                           # expression (expr-list), call
///               ::= cv <type> <expression>                       # type (expression), conversion with one argument
///               ::= cv <type> _ <expression>* E                  # type (expr-list), conversion with other than one argument
///               ::= tl <type> <expression>* E                    # type {expr-list}, conversion with braced-init-list argument
///               ::= il <expression> E                            # {expr-list}, braced-init-list in any other context
///               ::= [gs] nw <expression>* _ <type> E             # new (expr-list) type
///               ::= [gs] nw <expression>* _ <type> <initializer> # new (expr-list) type (init)
///               ::= [gs] na <expression>* _ <type> E             # new[] (expr-list) type
///               ::= [gs] na <expression>* _ <type> <initializer> # new[] (expr-list) type (init)
///               ::= [gs] dl <expression>                         # delete expression
///               ::= [gs] da <expression>                         # delete[] expression
///               ::= dc <type> <expression>                       # dynamic_cast<type> (expression)
///               ::= sc <type> <expression>                       # static_cast<type> (expression)
///               ::= cc <type> <expression>                       # const_cast<type> (expression)
///               ::= rc <type> <expression>                       # reinterpret_cast<type> (expression)
///               ::= ti <type>                                    # typeid (type)
///               ::= te <expression>                              # typeid (expression)
///               ::= st <type>                                    # sizeof (type)
///               ::= sz <expression>                              # sizeof (expression)
///               ::= at <type>                                    # alignof (type)
///               ::= az <expression>                              # alignof (expression)
///               ::= nx <expression>                              # noexcept (expression)
///               ::= <template-param>
///               ::= <function-param>
///               ::= dt <expression> <unresolved-name>            # expr.name
///               ::= pt <expression> <unresolved-name>            # expr->name
///               ::= ds <expression> <expression>                 # expr.*expr
///               ::= sZ <template-param>                          # sizeof...(T), size of a template parameter pack
///               ::= sZ <function-param>                          # sizeof...(parameter), size of a function parameter pack
///               ::= sP <template-arg>* E                         # sizeof...(T), size of a captured template parameter pack from an alias template
///               ::= sp <expression>                              # expression..., pack expansion
///               ::= tw <expression>                              # throw expression
///               ::= tr                                           # throw with no operand (rethrow)
///               ::= <unresolved-name>                            # f(p), N::f(p), ::f(p),
///                                                                # freestanding dependent name (e.g., T::x),
///                                                                # objectless nonstatic member reference
///               ::= <expr-primary>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Expression {
    /// A unary operator expression.
    Unary(OperatorName, Box<Expression>),

    /// A binary operator expression.
    Binary(OperatorName, Box<Expression>, Box<Expression>),

    /// A ternary operator expression.
    Ternary(OperatorName, Box<Expression>, Box<Expression>, Box<Expression>),

    /// A prefix `++`.
    PrefixInc(Box<Expression>),

    /// A prefix `--`.
    PrefixDec(Box<Expression>),

    /// A call with functor and arguments.
    Call(Box<Expression>, Vec<Expression>),

    /// A type conversion with one argument.
    ConversionOne(TypeHandle, Box<Expression>),

    /// A type conversion with many arguments.
    ConversionMany(TypeHandle, Vec<Expression>),

    /// A type conversion with many arguments.
    ConversionBraced(TypeHandle, Vec<Expression>),

    /// A braced init list expression.
    BracedInitList(Box<Expression>),

    /// The `new` operator.
    New(Vec<Expression>, TypeHandle, Option<Initializer>),

    /// The global `::new` operator.
    GlobalNew(Vec<Expression>, TypeHandle, Option<Initializer>),

    /// The `new[]` operator.
    NewArray(Vec<Expression>, TypeHandle, Option<Initializer>),

    /// The global `::new[]` operator.
    GlobalNewArray(Vec<Expression>, TypeHandle, Option<Initializer>),

    /// The `delete` operator.
    Delete(Box<Expression>),

    /// The global `::delete` operator.
    GlobalDelete(Box<Expression>),

    /// The `delete[]` operator.
    DeleteArray(Box<Expression>),

    /// The global `::delete[]` operator.
    GlobalDeleteArray(Box<Expression>),

    /// `dynamic_cast<type> (expression)`
    DynamicCast(TypeHandle, Box<Expression>),

    /// `static_cast<type> (expression)`
    StaticCast(TypeHandle, Box<Expression>),

    /// `const_cast<type> (expression)`
    ConstCast(TypeHandle, Box<Expression>),

    /// `reinterpret_cast<type> (expression)`
    ReinterpretCast(TypeHandle, Box<Expression>),

    /// `typeid (type)`
    TypeidType(TypeHandle),

    /// `typeid (expression)`
    TypeidExpr(Box<Expression>),

    /// `sizeof (type)`
    SizeofType(TypeHandle),

    /// `sizeof (expression)`
    SizeofExpr(Box<Expression>),

    /// `alignof (type)`
    AlignofType(TypeHandle),

    /// `alignof (expression)`
    AlignofExpr(Box<Expression>),

    /// `noexcept (expression)`
    Noexcept(Box<Expression>),

    /// A named template parameter.
    TemplateParam(TemplateParam),

    /// A function parameter.
    FunctionParam(FunctionParam),

    /// `expr.name`
    Member(Box<Expression>, UnresolvedName),

    /// `expr->name`
    DerefMember(Box<Expression>, UnresolvedName),

    /// `expr.*expr`
    PointerToMember(Box<Expression>, Box<Expression>),

    /// `sizeof...(T)`, size of a template parameter pack.
    SizeofTemplatePack(TemplateParam),

    /// `sizeof...(parameter)`, size of a function parameter pack.
    SizeofFunctionPack(FunctionParam),

    /// `sizeof...(T)`, size of a captured template parameter pack from an alias
    /// template.
    SizeofCapturedTemplatePack(Vec<TemplateArg>),

    /// `expression...`, pack expansion.
    PackExpansion(Box<Expression>),

    /// `throw expression`
    Throw(Box<Expression>),

    /// `throw` with no operand
    Rethrow,

    /// `f(p)`, `N::f(p)`, `::f(p)`, freestanding dependent name (e.g., `T::x`),
    /// objectless nonstatic member reference.
    UnresolvedName(UnresolvedName),

    /// An `<expr-primary>` production.
    Primary(ExprPrimary),
}

impl Parse for Expression {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Expression, IndexStr<'b>)> {
        log_parse!("Expression", input);

        if let Ok(tail) = consume(b"pp_", input) {
            let (expr, tail) = try!(Expression::parse(subs, tail));
            let expr = Expression::PrefixInc(Box::new(expr));
            return Ok((expr, tail));
        }

        if let Ok(tail) = consume(b"mm_", input) {
            let (expr, tail) = try!(Expression::parse(subs, tail));
            let expr = Expression::PrefixDec(Box::new(expr));
            return Ok((expr, tail));
        }

        if let Some((head, tail)) = input.try_split_at(2) {
            match head.as_ref() {
                b"cl" => {
                    let (func, tail) = try!(Expression::parse(subs, tail));
                    let (args, tail) = try!(zero_or_more::<Expression>(subs, tail));
                    let expr = Expression::Call(Box::new(func), args);
                    return Ok((expr, tail));
                }
                b"cv" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    if let Ok(tail) = consume(b"_", tail) {
                        let (exprs, tail) = try!(zero_or_more::<Expression>(subs, tail));
                        let tail = try!(consume(b"E", tail));
                        let expr = Expression::ConversionMany(ty, exprs);
                        return Ok((expr, tail));
                    } else {
                        let (expr, tail) = try!(Expression::parse(subs, tail));
                        let expr = Expression::ConversionOne(ty, Box::new(expr));
                        return Ok((expr, tail));
                    }
                }
                b"tl" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let (exprs, tail) = try!(zero_or_more::<Expression>(subs, tail));
                    let expr = Expression::ConversionBraced(ty, exprs);
                    let tail = try!(consume(b"E", tail));
                    return Ok((expr, tail));
                }
                b"il" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let tail = try!(consume(b"E", tail));
                    let expr = Expression::BracedInitList(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"dc" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::DynamicCast(ty, Box::new(expr));
                    return Ok((expr, tail));
                }
                b"sc" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::StaticCast(ty, Box::new(expr));
                    return Ok((expr, tail));
                }
                b"cc" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::ConstCast(ty, Box::new(expr));
                    return Ok((expr, tail));
                }
                b"rc" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::ReinterpretCast(ty, Box::new(expr));
                    return Ok((expr, tail));
                }
                b"ti" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let expr = Expression::TypeidType(ty);
                    return Ok((expr, tail));
                }
                b"te" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::TypeidExpr(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"st" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let expr = Expression::SizeofType(ty);
                    return Ok((expr, tail));
                }
                b"sz" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::SizeofExpr(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"at" => {
                    let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                    let expr = Expression::AlignofType(ty);
                    return Ok((expr, tail));
                }
                b"az" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::AlignofExpr(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"nx" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::Noexcept(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"dt" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let (name, tail) = try!(UnresolvedName::parse(subs, tail));
                    let expr = Expression::Member(Box::new(expr), name);
                    return Ok((expr, tail));
                }
                b"pt" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let (name, tail) = try!(UnresolvedName::parse(subs, tail));
                    let expr = Expression::DerefMember(Box::new(expr), name);
                    return Ok((expr, tail));
                }
                b"ds" => {
                    let (first, tail) = try!(Expression::parse(subs, tail));
                    let (second, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::PointerToMember(Box::new(first),
                                                           Box::new(second));
                    return Ok((expr, tail));
                }
                b"sZ" => {
                    if let Ok((param, tail)) = TemplateParam::parse(subs, tail) {
                        let expr = Expression::SizeofTemplatePack(param);
                        return Ok((expr, tail));
                    }

                    let (param, tail) = try!(FunctionParam::parse(subs, tail));
                    let expr = Expression::SizeofFunctionPack(param);
                    return Ok((expr, tail));
                }
                b"sP" => {
                    let (args, tail) = try!(zero_or_more::<TemplateArg>(subs, tail));
                    let expr = Expression::SizeofCapturedTemplatePack(args);
                    let tail = try!(consume(b"E", tail));
                    return Ok((expr, tail));
                }
                b"sp" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::PackExpansion(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"tw" => {
                    let (expr, tail) = try!(Expression::parse(subs, tail));
                    let expr = Expression::Throw(Box::new(expr));
                    return Ok((expr, tail));
                }
                b"tr" => {
                    let expr = Expression::Rethrow;
                    return Ok((expr, tail));
                }
                b"gs" => {
                    return can_be_global(true, subs, tail);
                }
                _ => {}
            }
        }

        if let Ok((expr, tail)) = can_be_global(false, subs, input) {
            return Ok((expr, tail));
        }

        if let Ok((param, tail)) = TemplateParam::parse(subs, input) {
            let expr = Expression::TemplateParam(param);
            return Ok((expr, tail));
        }

        if let Ok((param, tail)) = FunctionParam::parse(subs, input) {
            let expr = Expression::FunctionParam(param);
            return Ok((expr, tail));
        }

        if let Ok((name, tail)) = UnresolvedName::parse(subs, input) {
            let expr = Expression::UnresolvedName(name);
            return Ok((expr, tail));
        }

        if let Ok((prim, tail)) = ExprPrimary::parse(subs, input) {
            let expr = Expression::Primary(prim);
            return Ok((expr, tail));
        }

        // "A production for <expression> that directly specifies an operation
        // code (e.g., for the -> operator) takes precedence over one that is
        // expressed in terms of (unary/binary/ternary) <operator-name>." So try
        // and parse unary/binary/ternary expressions last.
        //
        // TODO: Should we check if the operator matches the arity here?
        let (opname, tail) = try!(OperatorName::parse(subs, input));
        let (first, tail) = try!(Expression::parse(subs, tail));
        return if let Ok((second, tail)) = Expression::parse(subs, tail) {
            if let Ok((third, tail)) = Expression::parse(subs, tail) {
                let expr = Expression::Ternary(opname,
                                               Box::new(first),
                                               Box::new(second),
                                               Box::new(third));
                Ok((expr, tail))
            } else {
                let expr = Expression::Binary(opname, Box::new(first), Box::new(second));
                Ok((expr, tail))
            }
        } else {
            let expr = Expression::Unary(opname, Box::new(first));
            Ok((expr, tail))
        };

        // Parse the various expressions that can optionally have a leading "gs"
        // to indicate that they are in the global namespace. The input is after
        // we have already detected consumed the optional "gs" and if we did
        // find it, then `is_global` should be true.
        fn can_be_global<'a, 'b>(is_global: bool,
                                 subs: &'a mut SubstitutionTable,
                                 input: IndexStr<'b>)
                                 -> Result<(Expression, IndexStr<'b>)> {
            match input.try_split_at(2) {
                None => Err(error::Error::UnexpectedEnd),
                Some((head, tail)) => {
                    match head.as_ref() {
                        b"nw" => {
                            let (exprs, tail) = try!(zero_or_more::<Expression>(subs,
                                                                                tail));
                            let tail = try!(consume(b"_", tail));
                            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                            if let Ok(tail) = consume(b"E", tail) {
                                let expr = if is_global {
                                    Expression::GlobalNew(exprs, ty, None)
                                } else {
                                    Expression::New(exprs, ty, None)
                                };
                                Ok((expr, tail))
                            } else {
                                let (init, tail) = try!(Initializer::parse(subs, tail));
                                let expr = if is_global {
                                    Expression::GlobalNew(exprs, ty, Some(init))
                                } else {
                                    Expression::New(exprs, ty, Some(init))
                                };
                                Ok((expr, tail))
                            }
                        }
                        b"na" => {
                            let (exprs, tail) = try!(zero_or_more::<Expression>(subs,
                                                                                tail));
                            let tail = try!(consume(b"_", tail));
                            let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                            if let Ok(tail) = consume(b"E", tail) {
                                let expr = if is_global {
                                    Expression::GlobalNewArray(exprs, ty, None)
                                } else {
                                    Expression::NewArray(exprs, ty, None)
                                };
                                Ok((expr, tail))
                            } else {
                                let (init, tail) = try!(Initializer::parse(subs, tail));
                                let expr = if is_global {
                                    Expression::GlobalNewArray(exprs, ty, Some(init))
                                } else {
                                    Expression::NewArray(exprs, ty, Some(init))
                                };
                                Ok((expr, tail))
                            }
                        }
                        b"dl" => {
                            let (expr, tail) = try!(Expression::parse(subs, tail));
                            let expr = if is_global {
                                Expression::GlobalDelete(Box::new(expr))
                            } else {
                                Expression::Delete(Box::new(expr))
                            };
                            Ok((expr, tail))
                        }
                        b"da" => {
                            let (expr, tail) = try!(Expression::parse(subs, tail));
                            let expr = if is_global {
                                Expression::GlobalDeleteArray(Box::new(expr))
                            } else {
                                Expression::DeleteArray(Box::new(expr))
                            };
                            Ok((expr, tail))
                        }
                        _ => Err(error::Error::UnexpectedText),
                    }
                }
            }
        }
    }
}

impl Demangle for Expression {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        // TODO: do we need to actually understand operator precedence?
        match *self {
            Expression::Unary(ref op, ref expr) => {
                try!(op.demangle(ctx, stack));
                try!(write!(ctx, " "));
                expr.demangle(ctx, stack)
            }
            Expression::Binary(ref op, ref lhs, ref rhs) => {
                try!(write!(ctx, "("));
                try!(lhs.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                try!(op.demangle(ctx, stack));
                try!(write!(ctx, "("));
                try!(rhs.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::Ternary(OperatorName::Question,
                                ref condition,
                                ref consequent,
                                ref alternative) => {
                try!(condition.demangle(ctx, stack));
                try!(write!(ctx, " ? "));
                try!(consequent.demangle(ctx, stack));
                try!(write!(ctx, " : "));
                alternative.demangle(ctx, stack)
            }
            Expression::Ternary(ref op, ref e1, ref e2, ref e3) => {
                // Nonsensical ternary operator? Just print it like a function call,
                // I suppose...
                //
                // TODO: should we detect and reject this during parsing?
                try!(op.demangle(ctx, stack));
                try!(write!(ctx, "("));
                try!(e1.demangle(ctx, stack));
                try!(write!(ctx, ", "));
                try!(e2.demangle(ctx, stack));
                try!(write!(ctx, ", "));
                try!(e3.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::PrefixInc(ref expr) => {
                try!(write!(ctx, "++"));
                expr.demangle(ctx, stack)
            }
            Expression::PrefixDec(ref expr) => {
                try!(write!(ctx, "--"));
                expr.demangle(ctx, stack)
            }
            Expression::Call(ref functor_expr, ref args) => {
                try!(write!(ctx, "("));
                try!(functor_expr.demangle(ctx, stack));
                try!(write!(ctx, ")("));
                let mut need_comma = false;
                for arg in args {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(arg.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::ConversionOne(ref ty, ref expr) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, "("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::ConversionMany(ref ty, ref exprs) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, "("));
                let mut need_comma = false;
                for expr in exprs {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(expr.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::ConversionBraced(ref ty, ref exprs) => {
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, "{{"));
                let mut need_comma = false;
                for expr in exprs {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(expr.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, "}}"));
                Ok(())
            }
            Expression::BracedInitList(ref expr) => {
                try!(write!(ctx, "{{"));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, "}}"));
                Ok(())
            }
            // TODO: factor out all this duplication in the `new` variants.
            Expression::New(ref exprs, ref ty, ref init) => {
                try!(write!(ctx, "new ("));
                let mut need_comma = false;
                for expr in exprs {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(expr.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ") "));
                try!(ty.demangle(ctx, stack));
                if let Some(ref init) = *init {
                    try!(init.demangle(ctx, stack));
                }
                Ok(())
            }
            Expression::GlobalNew(ref exprs, ref ty, ref init) => {
                try!(write!(ctx, "::new ("));
                let mut need_comma = false;
                for expr in exprs {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(expr.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ") "));
                try!(ty.demangle(ctx, stack));
                if let Some(ref init) = *init {
                    try!(init.demangle(ctx, stack));
                }
                Ok(())
            }
            Expression::NewArray(ref exprs, ref ty, ref init) => {
                try!(write!(ctx, "new[] ("));
                let mut need_comma = false;
                for expr in exprs {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(expr.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ") "));
                try!(ty.demangle(ctx, stack));
                if let Some(ref init) = *init {
                    try!(init.demangle(ctx, stack));
                }
                Ok(())
            }
            Expression::GlobalNewArray(ref exprs, ref ty, ref init) => {
                try!(write!(ctx, "::new[] ("));
                let mut need_comma = false;
                for expr in exprs {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(expr.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ") "));
                try!(ty.demangle(ctx, stack));
                if let Some(ref init) = *init {
                    try!(init.demangle(ctx, stack));
                }
                Ok(())
            }
            Expression::Delete(ref expr) => {
                try!(write!(ctx, "delete "));
                expr.demangle(ctx, stack)
            }
            Expression::GlobalDelete(ref expr) => {
                try!(write!(ctx, "::delete "));
                expr.demangle(ctx, stack)
            }
            Expression::DeleteArray(ref expr) => {
                try!(write!(ctx, "delete[] "));
                expr.demangle(ctx, stack)
            }
            Expression::GlobalDeleteArray(ref expr) => {
                try!(write!(ctx, "::delete[] "));
                expr.demangle(ctx, stack)
            }
            // TODO: factor out duplicated code from cast variants.
            Expression::DynamicCast(ref ty, ref expr) => {
                try!(write!(ctx, "dynamic_cast<"));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ">("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::StaticCast(ref ty, ref expr) => {
                try!(write!(ctx, "static_cast<"));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ">("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::ConstCast(ref ty, ref expr) => {
                try!(write!(ctx, "const_cast<"));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ">("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::ReinterpretCast(ref ty, ref expr) => {
                try!(write!(ctx, "reinterpret_cast<"));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ">("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::TypeidType(ref ty) => {
                try!(write!(ctx, "typeid ("));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::TypeidExpr(ref expr) => {
                try!(write!(ctx, "typeid ("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::SizeofType(ref ty) => {
                try!(write!(ctx, "sizeof ("));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::SizeofExpr(ref expr) => {
                try!(write!(ctx, "sizeof ("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::AlignofType(ref ty) => {
                try!(write!(ctx, "alignof ("));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::AlignofExpr(ref expr) => {
                try!(write!(ctx, "alignof ("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::Noexcept(ref expr) => {
                try!(write!(ctx, "noexcept ("));
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::TemplateParam(ref param) => param.demangle(ctx, stack),
            Expression::FunctionParam(ref param) => param.demangle(ctx, stack),
            Expression::Member(ref expr, ref name) => {
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, "."));
                name.demangle(ctx, stack)
            }
            Expression::DerefMember(ref expr, ref name) => {
                try!(expr.demangle(ctx, stack));
                try!(write!(ctx, "->"));
                name.demangle(ctx, stack)
            }
            Expression::PointerToMember(ref e1, ref e2) => {
                try!(e1.demangle(ctx, stack));
                try!(write!(ctx, ".*"));
                e2.demangle(ctx, stack)
            }
            Expression::SizeofTemplatePack(ref param) => {
                try!(write!(ctx, "sizeof...("));
                try!(param.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::SizeofFunctionPack(ref param) => {
                try!(write!(ctx, "sizeof...("));
                try!(param.demangle(ctx, stack));
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::SizeofCapturedTemplatePack(ref args) => {
                try!(write!(ctx, "sizeof...("));
                let mut need_comma = false;
                for arg in args {
                    if need_comma {
                        try!(write!(ctx, ", "));
                    }
                    try!(arg.demangle(ctx, stack));
                    need_comma = true;
                }
                try!(write!(ctx, ")"));
                Ok(())
            }
            Expression::PackExpansion(ref pack) => {
                try!(pack.demangle(ctx, stack));
                try!(write!(ctx, "..."));
                Ok(())
            }
            Expression::Throw(ref expr) => {
                try!(write!(ctx, "throw "));
                expr.demangle(ctx, stack)
            }
            Expression::Rethrow => {
                try!(write!(ctx, "throw"));
                Ok(())
            }
            Expression::UnresolvedName(ref name) => name.demangle(ctx, stack),
            Expression::Primary(ref expr) => expr.demangle(ctx, stack),
        }
    }
}

/// The `<unresolved-name>` production.
///
/// ```text
/// <unresolved-name> ::= [gs] <base-unresolved-name>
///                          #
///                   ::= sr <unresolved-type> <base-unresolved-name>
///                          #
///                   ::= srN <unresolved-type> <unresolved-qualifier-level>+ E <base-unresolved-name>
///                          #
///                   ::= [gs] sr <unresolved-qualifier-level>+ E <base-unresolved-name>
///                          # A::x, N::y, A<T>::z; "gs" means leading "::"
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum UnresolvedName {
    /// `x`
    Name(BaseUnresolvedName),

    /// `::x`
    Global(BaseUnresolvedName),

    /// `T::x`  or `decltype(p)::x` or `T::N::x` or `decltype(p)::N::x`
    Nested1(UnresolvedTypeHandle, Vec<UnresolvedQualifierLevel>, BaseUnresolvedName),

    /// `A::x` or `N::y` or `A<T>::z`
    Nested2(Vec<UnresolvedQualifierLevel>, BaseUnresolvedName),

    /// `::A::x` or `::N::y` or `::A<T>::z`
    GlobalNested2(Vec<UnresolvedQualifierLevel>, BaseUnresolvedName),
}

impl Parse for UnresolvedName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnresolvedName, IndexStr<'b>)> {
        log_parse!("UnresolvedName", input);

        if let Ok(tail) = consume(b"gs", input) {
            if let Ok((name, tail)) = BaseUnresolvedName::parse(subs, tail) {
                return Ok((UnresolvedName::Global(name), tail));
            }

            let tail = try!(consume(b"sr", tail));
            let (levels, tail) = try!(one_or_more::<UnresolvedQualifierLevel>(subs,
                                                                              tail));
            let tail = try!(consume(b"E", tail));
            let (name, tail) = try!(BaseUnresolvedName::parse(subs, tail));
            return Ok((UnresolvedName::GlobalNested2(levels, name), tail));
        }

        if let Ok((name, tail)) = BaseUnresolvedName::parse(subs, input) {
            return Ok((UnresolvedName::Name(name), tail));
        }

        let tail = try!(consume(b"sr", input));

        if tail.peek() == Some(b'N') {
            let tail = consume(b"N", tail).unwrap();
            let (ty, tail) = try!(UnresolvedTypeHandle::parse(subs, tail));
            let (levels, tail) = try!(one_or_more::<UnresolvedQualifierLevel>(subs,
                                                                              tail));
            let tail = try!(consume(b"E", tail));
            let (name, tail) = try!(BaseUnresolvedName::parse(subs, tail));
            return Ok((UnresolvedName::Nested1(ty, levels, name), tail));
        }

        if let Ok((ty, tail)) = UnresolvedTypeHandle::parse(subs, tail) {
            let (name, tail) = try!(BaseUnresolvedName::parse(subs, tail));
            return Ok((UnresolvedName::Nested1(ty, vec![], name), tail));
        }

        let (levels, tail) = try!(one_or_more::<UnresolvedQualifierLevel>(subs, tail));
        let tail = try!(consume(b"E", tail));
        let (name, tail) = try!(BaseUnresolvedName::parse(subs, tail));
        Ok((UnresolvedName::Nested2(levels, name), tail))
    }
}

impl Demangle for UnresolvedName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            UnresolvedName::Name(ref name) => name.demangle(ctx, stack),
            UnresolvedName::Global(ref name) => {
                try!(write!(ctx, "::"));
                name.demangle(ctx, stack)
            }
            UnresolvedName::Nested1(ref ty, ref levels, ref name) => {
                try!(ty.demangle(ctx, stack));
                for lvl in &levels[..] {
                    try!(write!(ctx, "::"));
                    try!(lvl.demangle(ctx, stack));
                }
                name.demangle(ctx, stack)
            }
            UnresolvedName::Nested2(ref levels, ref name) => {
                for lvl in &levels[..] {
                    try!(write!(ctx, "::"));
                    try!(lvl.demangle(ctx, stack));
                }
                name.demangle(ctx, stack)
            }
            /// `::A::x` or `::N::y` or `::A<T>::z`
            UnresolvedName::GlobalNested2(ref levels, ref name) => {
                try!(write!(ctx, "::"));
                for lvl in &levels[..] {
                    try!(write!(ctx, "::"));
                    try!(lvl.demangle(ctx, stack));
                }
                name.demangle(ctx, stack)
            }
        }
    }
}

/// The `<unresolved-type>` production.
///
/// ```text
/// <unresolved-type> ::= <template-param> [ <template-args> ]  # T:: or T<X,Y>::
///                   ::= <decltype>                            # decltype(p)::
///                   ::= <substitution>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum UnresolvedType {
    /// An unresolved template type.
    Template(TemplateParam, Option<TemplateArgs>),

    /// An unresolved `decltype`.
    Decltype(Decltype),
}

define_handle! {
    /// A reference to a parsed `<unresolved-type>` production.
    pub enum UnresolvedTypeHandle
}

impl Parse for UnresolvedTypeHandle {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnresolvedTypeHandle, IndexStr<'b>)> {
        log_parse!("UnresolvedTypeHandle", input);

        if let Ok((param, tail)) = TemplateParam::parse(subs, input) {
            let (args, tail) = if let Ok((args, tail)) = TemplateArgs::parse(subs,
                                                                             tail) {
                (Some(args), tail)
            } else {
                (None, tail)
            };
            let ty = UnresolvedType::Template(param, args);
            let ty = Substitutable::UnresolvedType(ty);
            let idx = subs.insert(ty);
            let handle = UnresolvedTypeHandle::BackReference(idx);
            return Ok((handle, tail));
        }

        if let Ok((decltype, tail)) = Decltype::parse(subs, input) {
            let ty = UnresolvedType::Decltype(decltype);
            let ty = Substitutable::UnresolvedType(ty);
            let idx = subs.insert(ty);
            let handle = UnresolvedTypeHandle::BackReference(idx);
            return Ok((handle, tail));
        }

        let (sub, tail) = try!(Substitution::parse(subs, input));
        match sub {
            Substitution::WellKnown(component) => {
                Ok((UnresolvedTypeHandle::WellKnown(component), tail))
            }
            Substitution::BackReference(idx) => {
                // TODO: should this check that the back reference actually
                // points to an `<unresolved-type>`?
                Ok((UnresolvedTypeHandle::BackReference(idx), tail))
            }
        }
    }
}

impl Demangle for UnresolvedType {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            UnresolvedType::Decltype(ref dt) => dt.demangle(ctx, stack),
            UnresolvedType::Template(ref param, ref args) => {
                if let Some(ref args) = *args {
                    let stack = stack.push(args);
                    try!(param.demangle(ctx, stack));
                    try!(args.demangle(ctx, stack));
                } else {
                    try!(param.demangle(ctx, stack));
                }
                Ok(())
            }
        }
    }
}

/// The `<unresolved-qualifier-level>` production.
///
/// ```text
/// <unresolved-qualifier-level> ::= <simple-id>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct UnresolvedQualifierLevel(SimpleId);

impl Parse for UnresolvedQualifierLevel {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(UnresolvedQualifierLevel, IndexStr<'b>)> {
        log_parse!("UnresolvedQualifierLevel", input);

        let (id, tail) = try!(SimpleId::parse(subs, input));
        Ok((UnresolvedQualifierLevel(id), tail))
    }
}

impl Demangle for UnresolvedQualifierLevel {
    #[inline]
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        self.0.demangle(ctx, stack)
    }
}

/// The `<simple-id>` production.
///
/// ```text
/// <simple-id> ::= <source-name> [ <template-args> ]
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct SimpleId(SourceName, Option<TemplateArgs>);

impl Parse for SimpleId {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(SimpleId, IndexStr<'b>)> {
        log_parse!("SimpleId", input);

        let (name, tail) = try!(SourceName::parse(subs, input));
        let (args, tail) = if let Ok((args, tail)) = TemplateArgs::parse(subs, tail) {
            (Some(args), tail)
        } else {
            (None, tail)
        };
        Ok((SimpleId(name, args), tail))
    }
}

impl Demangle for SimpleId {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(self.0.demangle(ctx, stack));
        if let Some(ref args) = self.1 {
            try!(args.demangle(ctx, stack));
        }
        Ok(())
    }
}

/// The `<base-unresolved-name>` production.
///
/// ```text
/// <base-unresolved-name> ::= <simple-id>                        # unresolved name
///                        ::= on <operator-name>                 # unresolved operator-function-id
///                        ::= on <operator-name> <template-args> # unresolved operator template-id
///                        ::= dn <destructor-name>               # destructor or pseudo-destructor;
///                                                               # e.g. ~X or ~X<N-1>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum BaseUnresolvedName {
    /// An unresolved name.
    Name(SimpleId),

    /// An unresolved function or template function name.
    Operator(OperatorName, Option<TemplateArgs>),

    /// An unresolved destructor name.
    Destructor(DestructorName),
}

impl Parse for BaseUnresolvedName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(BaseUnresolvedName, IndexStr<'b>)> {
        log_parse!("BaseUnresolvedName", input);

        if let Ok((name, tail)) = SimpleId::parse(subs, input) {
            return Ok((BaseUnresolvedName::Name(name), tail));
        }

        if let Ok(tail) = consume(b"on", input) {
            let (opname, tail) = try!(OperatorName::parse(subs, tail));
            let (args, tail) = if let Ok((args, tail)) = TemplateArgs::parse(subs,
                                                                             tail) {
                (Some(args), tail)
            } else {
                (None, tail)
            };
            return Ok((BaseUnresolvedName::Operator(opname, args), tail));
        }

        let tail = try!(consume(b"dn", input));
        let (name, tail) = try!(DestructorName::parse(subs, tail));
        Ok((BaseUnresolvedName::Destructor(name), tail))
    }
}

impl Demangle for BaseUnresolvedName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            BaseUnresolvedName::Name(ref name) => name.demangle(ctx, stack),
            BaseUnresolvedName::Destructor(ref dtor) => dtor.demangle(ctx, stack),
            BaseUnresolvedName::Operator(ref op, ref args) => {
                try!(op.demangle(ctx, stack));
                if let Some(ref args) = *args {
                    try!(args.demangle(ctx, stack));
                }
                Ok(())
            }
        }
    }
}

/// The `<destructor-name>` production.
///
/// ```text
/// <destructor-name> ::= <unresolved-type> # e.g., ~T or ~decltype(f())
///                   ::= <simple-id>       # e.g., ~A<2*N>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum DestructorName {
    /// A destructor for an unresolved type.
    Unresolved(UnresolvedTypeHandle),

    /// A destructor for a resolved type name.
    Name(SimpleId),
}

impl Parse for DestructorName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(DestructorName, IndexStr<'b>)> {
        log_parse!("DestructorName", input);

        if let Ok((ty, tail)) = UnresolvedTypeHandle::parse(subs, input) {
            return Ok((DestructorName::Unresolved(ty), tail));
        }

        let (name, tail) = try!(SimpleId::parse(subs, input));
        Ok((DestructorName::Name(name), tail))
    }
}

impl Demangle for DestructorName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "~"));
        match *self {
            DestructorName::Unresolved(ref ty) => ty.demangle(ctx, stack),
            DestructorName::Name(ref name) => name.demangle(ctx, stack),
        }
    }
}

/// The `<expr-primary>` production.
///
/// ```text
/// <expr-primary> ::= L <type> <value number> E                        # integer literal
///                ::= L <type> <value float> E                         # floating literal
///                ::= L <string type> E                                # string literal
///                ::= L <nullptr type> E                               # nullptr literal (i.e., "LDnE")
///                ::= L <pointer type> 0 E                             # null pointer template argument
///                ::= L <type> <real-part float> _ <imag-part float> E # complex floating point literal (C 2000)
///                ::= L <mangled-name> E                               # external name
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum ExprPrimary {
    /// A type literal.
    Literal(TypeHandle, usize, usize),

    /// An external name.
    External(MangledName),
}

impl Parse for ExprPrimary {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(ExprPrimary, IndexStr<'b>)> {
        log_parse!("ExprPrimary", input);

        let tail = try!(consume(b"L", input));

        if let Ok((ty, tail)) = TypeHandle::parse(subs, tail) {
            let start = tail.index();
            let num_bytes_in_literal = tail.as_ref()
                .iter()
                .take_while(|&&c| c != b'E')
                .count();
            let tail = tail.range_from(num_bytes_in_literal..);
            let end = tail.index();
            let tail = try!(consume(b"E", tail));
            let expr = ExprPrimary::Literal(ty, start, end);
            return Ok((expr, tail));
        }

        let (name, tail) = try!(MangledName::parse(subs, tail));
        let tail = try!(consume(b"E", tail));
        let expr = ExprPrimary::External(name);
        Ok((expr, tail))
    }
}

impl Demangle for ExprPrimary {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            ExprPrimary::External(ref name) => name.demangle(ctx, stack),
            ExprPrimary::Literal(
                TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Nullptr)),
                _,
                _) => {
                try!(write!(ctx, "nullptr"));
                return Ok(());
            }
            ExprPrimary::Literal(ref type_handle, start, end) => {
                debug_assert!(start <= end);
                if start == end {
                    type_handle.demangle(ctx, stack)
                } else {
                    try!(write!(ctx,
                                "{}",
                                String::from_utf8_lossy(&ctx.input[start..end])));
                    Ok(())
                }
            }
        }
    }
}

/// The `<initializer>` production.
///
/// ```text
/// <initializer> ::= pi <expression>* E # parenthesized initialization
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Initializer(Vec<Expression>);

impl Parse for Initializer {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Initializer, IndexStr<'b>)> {
        log_parse!("Initializer", input);

        let tail = try!(consume(b"pi", input));
        let (exprs, tail) = try!(zero_or_more::<Expression>(subs, tail));
        let tail = try!(consume(b"E", tail));
        Ok((Initializer(exprs), tail))
    }
}

impl Demangle for Initializer {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "("));
        let mut need_comma = false;
        for expr in &self.0 {
            if need_comma {
                try!(write!(ctx, ", "));
            }
            try!(expr.demangle(ctx, stack));
            need_comma = true;
        }
        try!(write!(ctx, ")"));
        Ok(())
    }
}

/// The `<local-name>` production.
///
/// ```text
/// <local-name> := Z <function encoding> E <entity name> [<discriminator>]
///              := Z <function encoding> E s [<discriminator>]
///              := Z <function encoding> Ed [ <parameter number> ] _ <entity name>
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum LocalName {
    /// The mangling of the enclosing function, the mangling of the entity
    /// relative to the function, and an optional discriminator.
    Relative(Box<Encoding>, Option<Box<Name>>, Option<Discriminator>),

    /// A default argument in a class definition.
    Default(Box<Encoding>, Option<usize>, Box<Name>),
}

impl Parse for LocalName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(LocalName, IndexStr<'b>)> {
        log_parse!("LocalName", input);

        let tail = try!(consume(b"Z", input));
        let (encoding, tail) = try!(Encoding::parse(subs, tail));
        let tail = try!(consume(b"E", tail));

        if let Ok(tail) = consume(b"s", tail) {
            let (disc, tail) = if let Ok((disc, tail)) = Discriminator::parse(subs,
                                                                              tail) {
                (Some(disc), tail)
            } else {
                (None, tail)
            };
            return Ok((LocalName::Relative(Box::new(encoding), None, disc), tail));
        }

        if let Ok(tail) = consume(b"d", tail) {
            let (param, tail) = if let Ok((num, tail)) = Number::parse(subs, tail) {
                (Some(num as _), tail)
            } else {
                (None, tail)
            };
            let tail = try!(consume(b"_", tail));
            let (name, tail) = try!(Name::parse(subs, tail));
            return Ok((LocalName::Default(Box::new(encoding), param, Box::new(name)),
                       tail));
        }

        let (name, tail) = try!(Name::parse(subs, tail));
        let (disc, tail) = if let Ok((disc, tail)) = Discriminator::parse(subs, tail) {
            (Some(disc), tail)
        } else {
            (None, tail)
        };

        Ok((LocalName::Relative(Box::new(encoding), Some(Box::new(name)), disc), tail))
    }
}

impl Demangle for LocalName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            LocalName::Relative(ref encoding, Some(ref name), _) => {
                try!(encoding.demangle(ctx, stack));
                try!(write!(ctx, "::"));
                name.demangle(ctx, stack)
            }
            LocalName::Relative(ref encoding, None, _) => {
                // No name means that this is the symbol for a string literal.
                try!(encoding.demangle(ctx, stack));
                try!(write!(ctx, "::string literal"));
                Ok(())
            }
            LocalName::Default(ref encoding, _, _) => encoding.demangle(ctx, stack),
        }
    }
}

impl GetTemplateArgs for LocalName {
    fn get_template_args<'a>(&'a self,
                             subs: &'a SubstitutionTable)
                             -> Option<&'a TemplateArgs> {
        match *self {
            LocalName::Relative(_, None, _) => None,
            LocalName::Relative(_, Some(ref name), _) => name.get_template_args(subs),
            LocalName::Default(_, _, ref name) => name.get_template_args(subs),
        }
    }
}

/// The `<discriminator>` production.
///
/// ```text
/// <discriminator> := _ <non-negative number>      # when number < 10
///                 := __ <non-negative number> _   # when number >= 10
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct Discriminator(usize);

impl Parse for Discriminator {
    fn parse<'a, 'b>(_subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Discriminator, IndexStr<'b>)> {
        log_parse!("Discriminator", input);

        let tail = try!(consume(b"_", input));

        if let Ok(tail) = consume(b"_", tail) {
            let (num, tail) = try!(parse_number(10, false, tail));
            debug_assert!(num >= 0);
            if num < 10 {
                return Err(error::Error::UnexpectedText);
            }
            let tail = try!(consume(b"_", tail));
            return Ok((Discriminator(num as _), tail));
        }

        match tail.try_split_at(1) {
            None => Err(error::Error::UnexpectedEnd),
            Some((head, tail)) => {
                match head.as_ref()[0] {
                    b'0' => Ok((Discriminator(0), tail)),
                    b'1' => Ok((Discriminator(1), tail)),
                    b'2' => Ok((Discriminator(2), tail)),
                    b'3' => Ok((Discriminator(3), tail)),
                    b'4' => Ok((Discriminator(4), tail)),
                    b'5' => Ok((Discriminator(5), tail)),
                    b'6' => Ok((Discriminator(6), tail)),
                    b'7' => Ok((Discriminator(7), tail)),
                    b'8' => Ok((Discriminator(8), tail)),
                    b'9' => Ok((Discriminator(9), tail)),
                    _ => Err(error::Error::UnexpectedText),
                }
            }
        }
    }
}

/// The `<closure-type-name>` production.
///
/// ```text
/// <closure-type-name> ::= Ul <lambda-sig> E [ <nonnegative number> ] _
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct ClosureTypeName(LambdaSig, Option<usize>);

impl Parse for ClosureTypeName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(ClosureTypeName, IndexStr<'b>)> {
        log_parse!("ClosureTypeName", input);

        let tail = try!(consume(b"Ul", input));
        let (sig, tail) = try!(LambdaSig::parse(subs, tail));
        let tail = try!(consume(b"E", tail));
        let (num, tail) = if let Ok((num, tail)) = parse_number(10, false, tail) {
            (Some(num as _), tail)
        } else {
            (None, tail)
        };
        let tail = try!(consume(b"_", tail));
        Ok((ClosureTypeName(sig, num), tail))
    }
}

impl Demangle for ClosureTypeName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        try!(write!(ctx, "{{lambda("));
        try!(self.0.demangle(ctx, stack));
        try!(write!(ctx, ")#{}}}", self.1.map_or(0, |n| n + 1)));
        Ok(())
    }
}

/// The `<lambda-sig>` production.
///
/// ```text
/// <lambda-sig> ::= <parameter type>+  # Parameter types or "v" if the lambda has no parameters
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct LambdaSig(Vec<TypeHandle>);

impl Parse for LambdaSig {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(LambdaSig, IndexStr<'b>)> {
        log_parse!("LambdaSig", input);

        let (types, tail) = if let Ok(tail) = consume(b"v", input) {
            (vec![], tail)
        } else {
            try!(one_or_more::<TypeHandle>(subs, input))
        };
        Ok((LambdaSig(types), tail))
    }
}

impl Demangle for LambdaSig {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        let mut need_comma = false;
        for ty in &self.0 {
            if need_comma {
                try!(write!(ctx, ", "));
            }
            try!(ty.demangle(ctx, stack));
            need_comma = true;
        }
        Ok(())
    }
}

/// The `<data-member-prefix>` production.
///
/// ```text
/// <data-member-prefix> := <member source-name> M
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub struct DataMemberPrefix(SourceName);

impl Parse for DataMemberPrefix {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(DataMemberPrefix, IndexStr<'b>)> {
        log_parse!("DataMemberPrefix", input);

        let (name, tail) = try!(SourceName::parse(subs, input));
        let tail = try!(consume(b"M", tail));
        Ok((DataMemberPrefix(name), tail))
    }
}

impl StartsWith for DataMemberPrefix {
    fn starts_with(byte: u8) -> bool {
        SourceName::starts_with(byte)
    }
}

impl Demangle for DataMemberPrefix {
    #[inline]
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        self.0.demangle(ctx, stack)
    }
}

/// The `<substitution>` form: a back-reference to some component we've already
/// parsed.
///
/// ```text
/// <substitution> ::= S <seq-id> _
///                ::= S_
///                ::= St # ::std::
///                ::= Sa # ::std::allocator
///                ::= Sb # ::std::basic_string
///                ::= Ss # ::std::basic_string < char,
///                                               ::std::char_traits<char>,
///                                               ::std::allocator<char> >
///                ::= Si # ::std::basic_istream<char,  std::char_traits<char> >
///                ::= So # ::std::basic_ostream<char,  std::char_traits<char> >
///                ::= Sd # ::std::basic_iostream<char, std::char_traits<char> >
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum Substitution {
    /// A reference to an entity that already occurred, ie the `S_` and `S
    /// <seq-id> _` forms.
    BackReference(usize),

    /// A well-known substitution component. These are the components that do
    /// not appear in the substitution table, but have abbreviations specified
    /// directly in the grammar.
    WellKnown(WellKnownComponent),
}

impl Parse for Substitution {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(Substitution, IndexStr<'b>)> {
        log_parse!("Substitution", input);

        if let Ok((well_known, tail)) = WellKnownComponent::parse(subs, input) {
            return Ok((Substitution::WellKnown(well_known), tail));
        }

        let tail = try!(consume(b"S", input));
        let (idx, tail) = if let Ok((idx, tail)) = SeqId::parse(subs, tail) {
            (idx.0 + 1, tail)
        } else {
            (0, tail)
        };

        if !subs.contains(idx) {
            return Err(error::Error::BadBackReference);
        }

        let tail = try!(consume(b"_", tail));
        log!("Found a reference to @ {}", idx);
        Ok((Substitution::BackReference(idx), tail))
    }
}

define_vocabulary! {
/// The `<substitution>` variants that are encoded directly in the grammar,
/// rather than as back references to other components in the substitution
/// table.
    #[derive(Clone, Debug, Hash, PartialEq, Eq)]
    pub enum WellKnownComponent {
        Std          (b"St", "std"),
        StdAllocator (b"Sa", "std::allocator"),
        StdString1   (b"Sb", "std::basic_string"),
        StdString2   (b"Ss", "std::string"),
        StdIstream   (b"Si", "std::basic_istream<char, std::char_traits<char> >"),
        StdOstream   (b"So", "std::ostream"),
        StdIostream  (b"Sd", "std::basic_iostream<char, std::char_traits<char> >")
    }
}

/// The `<special-name>` production.
///
/// The `<special-name>` production is spread in pieces through out the ABI
/// spec.
///
/// ### 5.1.4.1 Virtual Tables and RTTI
///
/// ```text
/// <special-name> ::= TV <type>    # virtual table
///                ::= TT <type>    # VTT structure (construction vtable index)
///                ::= TI <type>    # typeinfo structure
///                ::= TS <type>    # typeinfo name (null-terminated byte string)
/// ```
///
/// ### 5.1.4.2 Virtual Override Thunks
///
/// ```text
/// <special-name> ::= T <call-offset> <base encoding>
///     # base is the nominal target function of thunk
///
/// <special-name> ::= Tc <call-offset> <call-offset> <base encoding>
///     # base is the nominal target function of thunk
///     # first call-offset is 'this' adjustment
///     # second call-offset is result adjustment
/// ```
///
/// ### 5.1.4.4 Guard Variables
///
/// ```text
/// <special-name> ::= GV <object name> # Guard variable for one-time initialization
///     # No <type>
/// ```
///
/// ### 5.1.4.5 Lifetime-Extended Temporaries
///
/// ```text
/// <special-name> ::= GR <object name> _             # First temporary
/// <special-name> ::= GR <object name> <seq-id> _    # Subsequent temporaries
/// ```
#[derive(Clone, Debug, Hash, PartialEq, Eq)]
pub enum SpecialName {
    /// A virtual table.
    VirtualTable(TypeHandle),

    /// A VTT structure (construction vtable index).
    Vtt(TypeHandle),

    /// A typeinfo structure.
    Typeinfo(TypeHandle),

    /// A typeinfo name (null-terminated byte string).
    TypeinfoName(TypeHandle),

    /// A virtual override thunk.
    VirtualOverrideThunk(CallOffset, Box<Encoding>),

    /// A virtual override thunk with a covariant return type.
    VirtualOverrideThunkCovariant(CallOffset, CallOffset, Box<Encoding>),

    /// An initialization guard for some static storage.
    Guard(Name),

    /// A temporary used in the initialization of a static storage and promoted
    /// to a static lifetime.
    GuardTemporary(Name, usize),
}

impl Parse for SpecialName {
    fn parse<'a, 'b>(subs: &'a mut SubstitutionTable,
                     input: IndexStr<'b>)
                     -> Result<(SpecialName, IndexStr<'b>)> {
        log_parse!("SpecialName", input);

        let (head, tail) = match input.try_split_at(2) {
            None => return Err(error::Error::UnexpectedEnd),
            Some((head, tail)) => (head, tail),
        };

        match head.as_ref() {
            b"TV" => {
                let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                Ok((SpecialName::VirtualTable(ty), tail))
            }
            b"TT" => {
                let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                Ok((SpecialName::Vtt(ty), tail))
            }
            b"TI" => {
                let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                Ok((SpecialName::Typeinfo(ty), tail))
            }
            b"TS" => {
                let (ty, tail) = try!(TypeHandle::parse(subs, tail));
                Ok((SpecialName::TypeinfoName(ty), tail))
            }
            b"Tc" => {
                let (first, tail) = try!(CallOffset::parse(subs, tail));
                let (second, tail) = try!(CallOffset::parse(subs, tail));
                let (base, tail) = try!(Encoding::parse(subs, tail));
                Ok((SpecialName::VirtualOverrideThunkCovariant(first,
                                                               second,
                                                               Box::new(base)),
                    tail))
            }
            b"GV" => {
                let (name, tail) = try!(Name::parse(subs, tail));
                Ok((SpecialName::Guard(name), tail))
            }
            b"GR" => {
                let (name, tail) = try!(Name::parse(subs, tail));
                let (idx, tail) = if let Ok(tail) = consume(b"_", tail) {
                    (0, tail)
                } else {
                    let (idx, tail) = try!(SeqId::parse(subs, tail));
                    let tail = try!(consume(b"_", tail));
                    (idx.0 + 1, tail)
                };
                Ok((SpecialName::GuardTemporary(name, idx), tail))
            }
            _ => {
                if let Ok(tail) = consume(b"T", input) {
                    let (offset, tail) = try!(CallOffset::parse(subs, tail));
                    let (base, tail) = try!(Encoding::parse(subs, tail));
                    Ok((SpecialName::VirtualOverrideThunk(offset, Box::new(base)), tail))
                } else {
                    Err(error::Error::UnexpectedText)
                }
            }
        }
    }
}

impl Demangle for SpecialName {
    fn demangle<W>(&self,
                   ctx: &mut DemangleContext<W>,
                   stack: Option<ArgStack>)
                   -> io::Result<()>
        where W: io::Write
    {
        match *self {
            SpecialName::VirtualTable(ref ty) => {
                try!(write!(ctx, "{{vtable("));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ")}}"));
                Ok(())
            }
            SpecialName::Vtt(ref ty) => {
                try!(write!(ctx, "{{vtt("));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ")}}"));
                Ok(())
            }
            SpecialName::Typeinfo(ref ty) => {
                try!(write!(ctx, "typeinfo for "));
                ty.demangle(ctx, stack)
            }
            SpecialName::TypeinfoName(ref ty) => {
                try!(write!(ctx, "{{typeinfo name("));
                try!(ty.demangle(ctx, stack));
                try!(write!(ctx, ")}}"));
                Ok(())
            }
            SpecialName::VirtualOverrideThunk(ref offset, ref encoding) => {
                try!(write!(ctx, "{{virtual override thunk("));
                try!(offset.demangle(ctx, stack));
                try!(write!(ctx, ", "));
                try!(encoding.demangle(ctx, stack));
                try!(write!(ctx, ")}}"));
                Ok(())
            }
            SpecialName::VirtualOverrideThunkCovariant(ref this_offset,
                                                       ref result_offset,
                                                       ref encoding) => {
                try!(write!(ctx, "{{virtual override thunk("));
                try!(this_offset.demangle(ctx, stack));
                try!(write!(ctx, ", "));
                try!(result_offset.demangle(ctx, stack));
                try!(write!(ctx, ", "));
                try!(encoding.demangle(ctx, stack));
                try!(write!(ctx, ")}}"));
                Ok(())
            }
            SpecialName::Guard(ref name) => {
                try!(write!(ctx, "{{static initialization guard("));
                try!(name.demangle(ctx, stack));
                try!(write!(ctx, ")}}"));
                Ok(())
            }
            SpecialName::GuardTemporary(ref name, n) => {
                try!(write!(ctx, "{{static initialization guard temporary("));
                try!(name.demangle(ctx, stack));
                try!(write!(ctx, ", {})}}", n));
                Ok(())
            }
        }
    }
}

/// Expect and consume the given byte str, and return the advanced `IndexStr` if
/// we saw the expectation. Otherwise return an error of kind
/// `error::Error::UnexpectedText` if the input doesn't match, or
/// `error::Error::UnexpectedEnd` if it isn't long enough.
#[inline]
fn consume<'a>(expected: &[u8], input: IndexStr<'a>) -> Result<IndexStr<'a>> {
    match input.try_split_at(expected.len()) {
        Some((head, tail)) if head == expected => Ok(tail),
        Some(_) => Err(error::Error::UnexpectedText),
        None => Err(error::Error::UnexpectedEnd),
    }
}

fn one_or_more<'a, 'b, P>(subs: &'a mut SubstitutionTable,
                          input: IndexStr<'b>)
                          -> Result<(Vec<P>, IndexStr<'b>)>
    where P: Parse
{
    let (first, mut tail) = try!(P::parse(subs, input));
    let mut results = vec![first];
    loop {
        if let Ok((parsed, tail_tail)) = P::parse(subs, tail) {
            results.push(parsed);
            tail = tail_tail;
        } else {
            return Ok((results, tail));
        }
    }
}

fn zero_or_more<'a, 'b, P>(subs: &'a mut SubstitutionTable,
                           input: IndexStr<'b>)
                           -> Result<(Vec<P>, IndexStr<'b>)>
    where P: Parse
{
    let mut tail = input;
    let mut results = vec![];
    loop {
        if let Ok((parsed, tail_tail)) = P::parse(subs, tail) {
            results.push(parsed);
            tail = tail_tail;
        } else {
            return Ok((results, tail));
        }
    }
}

/// Parse a number with the given `base`. Do not allow negative numbers
/// (prefixed with an 'n' instead of a '-') if `allow_signed` is false.
#[allow(unsafe_code)]
fn parse_number(base: u32,
                allow_signed: bool,
                mut input: IndexStr)
                -> Result<(isize, IndexStr)> {
    if input.is_empty() {
        return Err(error::Error::UnexpectedEnd);
    }

    let num_is_negative = if allow_signed && input.as_ref()[0] == b'n' {
        input = input.range_from(1..);

        if input.is_empty() {
            return Err(error::Error::UnexpectedEnd);
        }

        true
    } else {
        false
    };

    let num_numeric = input.as_ref()
        .iter()
        .map(|&c| c as char)
        .take_while(|c| c.is_digit(base) && (c.is_numeric() || c.is_uppercase()))
        .count();
    if num_numeric == 0 {
        return Err(error::Error::UnexpectedText);
    }

    let (head, tail) = input.split_at(num_numeric);
    let head = head.as_ref();

    if num_numeric > 1 && head[0] == b'0' {
        // "<number>s appearing in mangled names never have leading zeroes,
        // except for the value zero, represented as '0'."
        return Err(error::Error::UnexpectedText);
    }

    let head = unsafe {
        // Safe because we know we only have valid numeric chars in this
        // slice, which are valid UTF-8.
        ::std::str::from_utf8_unchecked(head)
    };

    let mut number = try!(isize::from_str_radix(head, base)
        .map_err(|_| error::Error::Overflow));
    if num_is_negative {
        number = -number;
    }

    Ok((number, tail))
}

#[cfg(test)]
mod tests {
    use error::Error;
    use index_str::IndexStr;
    use std::fmt::Debug;
    use std::iter::FromIterator;
    use subs::{Substitutable, SubstitutionTable};
    use super::{ArrayType, BareFunctionType, BaseUnresolvedName, BuiltinType,
                CallOffset, ClassEnumType, ClosureTypeName, CtorDtorName, CvQualifiers,
                DataMemberPrefix, Decltype, Demangle, DemangleContext, DestructorName,
                Discriminator, Encoding, ExprPrimary, Expression, FunctionParam,
                FunctionType, Identifier, Initializer, LambdaSig, LocalName,
                MangledName, Name, NestedName, Number, NvOffset, OperatorName, Parse,
                PointerToMemberType, Prefix, PrefixHandle, RefQualifier, SeqId,
                SimpleId, SourceName, SpecialName, StandardBuiltinType, Substitution,
                TemplateArg, TemplateArgs, TemplateParam, TemplateTemplateParam,
                TemplateTemplateParamHandle, Type, TypeHandle, UnnamedTypeName,
                UnqualifiedName, UnresolvedName, UnresolvedQualifierLevel,
                UnresolvedType, UnresolvedTypeHandle, UnscopedName,
                UnscopedTemplateName, UnscopedTemplateNameHandle, VOffset,
                WellKnownComponent};

    fn assert_parse_ok<P, S1, S2, I1, I2>(production: &'static str,
                                          subs: S1,
                                          input: I1,
                                          expected: P,
                                          expected_tail: I2,
                                          expected_new_subs: S2)
        where P: Debug + Parse + PartialEq,
              S1: AsRef<[Substitutable]>,
              S2: AsRef<[Substitutable]>,
              I1: AsRef<[u8]>,
              I2: AsRef<[u8]>
    {
        let input = input.as_ref();
        let expected_tail = expected_tail.as_ref();

        let expected_subs = SubstitutionTable::from_iter(subs.as_ref()
            .iter()
            .cloned()
            .chain(expected_new_subs.as_ref().iter().cloned()));
        let mut subs = SubstitutionTable::from_iter(subs.as_ref().iter().cloned());

        match P::parse(&mut subs, IndexStr::from(input)) {
            Err(error) => {
                panic!("Parsing {:?} as {} failed: {}",
                       String::from_utf8_lossy(input),
                       production,
                       error)
            }
            Ok((value, tail)) => {
                if value != expected {
                    panic!("Parsing {:?} as {} produced\n\n{:#?}\n\nbut we expected\n\n{:#?}",
                           String::from_utf8_lossy(input),
                           production,
                           value,
                           expected);
                }
                if tail != expected_tail {
                    panic!("Parsing {:?} as {} left a tail of {:?}, expected {:?}",
                           String::from_utf8_lossy(input),
                           production,
                           tail,
                           String::from_utf8_lossy(expected_tail));
                }
                if subs != expected_subs {
                    panic!("Parsing {:?} as {} produced a substitutions table of\n\n\
                            {:#?}\n\n\
                            but we expected\n\n\
                            {:#?}",
                           String::from_utf8_lossy(input),
                           production,
                           subs,
                           expected_subs);
                }
            }
        }

        log!("=== assert_parse_ok PASSED ====================================");
    }

    fn simple_assert_parse_ok<P, I1, I2>(production: &'static str,
                                         input: I1,
                                         expected: P,
                                         expected_tail: I2)
        where P: Debug + Parse + PartialEq,
              I1: AsRef<[u8]>,
              I2: AsRef<[u8]>
    {
        assert_parse_ok::<P, _, _, _, _>(production,
                                         [],
                                         input,
                                         expected,
                                         expected_tail,
                                         []);
    }

    fn assert_parse_err<P, S, I>(production: &'static str,
                                 subs: S,
                                 input: I,
                                 expected_error: Error)
        where P: Debug + Parse + PartialEq,
              S: AsRef<[Substitutable]>,
              I: AsRef<[u8]>
    {
        let input = input.as_ref();
        let mut subs = SubstitutionTable::from_iter(subs.as_ref().iter().cloned());

        match P::parse(&mut subs, IndexStr::from(input)) {
            Err(ref error) if *error == expected_error => {}
            Err(ref error) => {
                panic!("Parsing {:?} as {} produced an error of kind {:?}, but we expected kind {:?}",
                       String::from_utf8_lossy(input),
                       production,
                       error,
                       expected_error);
            }
            Ok((value, tail)) => {
                panic!("Parsing {:?} as {} produced value\n\n{:#?}\n\nand tail {:?}, but we expected error kind {:?}",
                       String::from_utf8_lossy(input),
                       production,
                       value,
                       tail,
                       expected_error);
            }
        }

        log!("=== assert_parse_err PASSED ===================================");
    }

    fn simple_assert_parse_err<P, I>(production: &'static str,
                                     input: I,
                                     expected_error: Error)
        where P: Debug + Parse + PartialEq,
              I: AsRef<[u8]>
    {
        assert_parse_err::<P, _, _>(production, [], input, expected_error);
    }

    macro_rules! assert_parse {
        ( $production:ident {
            $( with subs $subs:expr => {
                Ok => {
                    $( $input:expr => {
                        $expected:expr ,
                        $expected_tail:expr ,
                        $expected_new_subs:expr
                    } )*
                }
                Err => {
                    $( $error_input:expr => $error:expr , )*
                }
            } )*
        } ) => {
            $( $(
                assert_parse_ok::<$production, _, _, _, _>(stringify!($production),
                                                           $subs,
                                                           $input,
                                                           $expected,
                                                           $expected_tail,
                                                           $expected_new_subs);
            )* )*

            $( $(
                assert_parse_err::<$production, _, _>(stringify!($production),
                                                      $subs,
                                                      $error_input,
                                                      $error);
            )* )*
        };

        ( $production:ident {
            Ok => {
                $( $input:expr => {
                    $expected:expr ,
                    $expected_tail:expr
                } )*
            }
            Err => {
                $( $error_input:expr => $error:expr , )*
            }
        } ) => {
            $(
                simple_assert_parse_ok::<$production, _, _>(stringify!($production),
                                                            $input,
                                                            $expected,
                                                            $expected_tail);
            )*


            $(
                simple_assert_parse_err::<$production, _>(stringify!($production),
                                                          $error_input,
                                                          $error);
            )*
        };
    }

    #[test]
    fn parse_mangled_name() {
        assert_parse!(MangledName {
            Ok => {
                b"_Z3foo..." => {
                    MangledName::Encoding(
                        Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 3,
                                            end: 6,
                                        })))))),
                    b"..."
                }
            }
            Err => {
                b"_Y" => Error::UnexpectedText,
                b"_Z" => Error::UnexpectedText,
                b"_" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_encoding() {
        assert_parse!(Encoding {
            with subs [] => {
                Ok => {
                    b"3fooi..." => {
                        Encoding::Function(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        })))),
                            BareFunctionType(vec![
                                TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Int))
                            ])),
                        b"...",
                        []
                    }
                    b"3foo..." => {
                        Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        }))))),
                        b"...",
                        []
                    }
                    b"GV3abc..." => {
                        Encoding::Special(
                            SpecialName::Guard(
                                Name::Unscoped(
                                    UnscopedName::Unqualified(
                                        UnqualifiedName::Source(
                                            SourceName(Identifier {
                                                start: 3,
                                                end: 6,
                                            })))))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_name() {
        assert_parse!(Name {
            with subs [
                Substitutable::Prefix(
                    Prefix::Unqualified(
                        UnqualifiedName::Operator(OperatorName::New))),
                Substitutable::Prefix(
                    Prefix::Nested(PrefixHandle::BackReference(0),
                                   UnqualifiedName::Operator(OperatorName::New))),
            ] => {
                Ok => {
                    b"NS0_E..." => {
                        Name::Nested(NestedName(CvQualifiers::default(),
                                                None,
                                                PrefixHandle::BackReference(1))),
                        b"...",
                        []
                    }
                    b"3abc..." => {
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 1,
                                        end: 4,
                                    })))),
                        b"...",
                        []
                    }
                    b"dlIcE..." => {
                        Name::UnscopedTemplate(
                            UnscopedTemplateNameHandle::BackReference(2),
                            TemplateArgs(vec![
                                TemplateArg::Type(
                                    TypeHandle::Builtin(
                                        BuiltinType::Standard(StandardBuiltinType::Char)))
                            ])),
                        b"...",
                        [
                            Substitutable::UnscopedTemplateName(
                                UnscopedTemplateName(
                                    UnscopedName::Unqualified(
                                        UnqualifiedName::Operator(
                                            OperatorName::Delete)))),
                        ]
                    }
                    b"Z3abcEs..." => {
                        Name::Local(
                            LocalName::Relative(
                                Box::new(Encoding::Data(
                                    Name::Unscoped(
                                        UnscopedName::Unqualified(
                                            UnqualifiedName::Source(
                                                SourceName(Identifier {
                                                    start: 2,
                                                    end: 5,
                                                })))))),
                                None,
                                None)),
                        b"...",
                        []
                    }
                    b"St3abc..." => {
                        Name::Std(UnqualifiedName::Source(SourceName(Identifier {
                            start: 3,
                            end: 6,
                        }))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_unscoped_template_name_handle() {
        assert_parse!(UnscopedTemplateNameHandle {
            with subs [
                Substitutable::UnscopedTemplateName(
                    UnscopedTemplateName(
                        UnscopedName::Unqualified(
                            UnqualifiedName::Operator(
                                OperatorName::New)))),
            ] => {
                Ok => {
                    b"S_..." => {
                        UnscopedTemplateNameHandle::BackReference(0),
                        b"...",
                        []
                    }
                    b"dl..." => {
                        UnscopedTemplateNameHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::UnscopedTemplateName(
                                UnscopedTemplateName(
                                    UnscopedName::Unqualified(
                                        UnqualifiedName::Operator(
                                            OperatorName::Delete))))
                        ]
                    }
                }
                Err => {
                    b"zzzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_nested_name() {
        // <nested-name> ::= N [<CV-qualifiers>] [<ref-qualifier>] <prefix> <unqualified-name> E
        //               ::= N [<CV-qualifiers>] [<ref-qualifier>] <template-prefix> <template-args> E
        assert_parse!(NestedName {
            with subs [
                Substitutable::Prefix(
                    Prefix::Unqualified(
                        UnqualifiedName::Operator(
                            OperatorName::New))),
            ] => {
                Ok => {
                    b"NKOS_3abcE..." => {
                        NestedName(
                            CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: true,
                            },
                            Some(RefQualifier::RValueRef),
                            PrefixHandle::BackReference(1)),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(0),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 6,
                                            end: 9,
                                        }))))
                        ]
                    }
                    b"NOS_3abcE..." => {
                        NestedName(
                            CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            Some(RefQualifier::RValueRef),
                            PrefixHandle::BackReference(1)),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(0),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 5,
                                            end: 8,
                                        }))))
                        ]
                    }
                    b"NS_3abcE..." => {
                        NestedName(
                            CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            None,
                            PrefixHandle::BackReference(1)),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(0),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 4,
                                            end: 7,
                                        }))))
                        ]
                    }
                    b"NKOS_3abcIJEEE..." => {
                        NestedName(
                            CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: true,
                            },
                            Some(RefQualifier::RValueRef),
                            PrefixHandle::BackReference(2)),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(0),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 6,
                                            end: 9,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::Template(
                                    PrefixHandle::BackReference(1),
                                    TemplateArgs(vec![TemplateArg::ArgPack(vec![])]))),
                        ]
                    }
                    b"NOS_3abcIJEEE..." => {
                        NestedName(
                            CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            Some(RefQualifier::RValueRef),
                            PrefixHandle::BackReference(2)),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(0),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 5,
                                            end: 8,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::Template(
                                    PrefixHandle::BackReference(1),
                                    TemplateArgs(vec![TemplateArg::ArgPack(vec![])]))),
                        ]
                    }
                    b"NS_3abcIJEEE..." => {
                        NestedName(
                            CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            None,
                            PrefixHandle::BackReference(2)),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(0),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 4,
                                            end: 7,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::Template(
                                    PrefixHandle::BackReference(1),
                                    TemplateArgs(vec![TemplateArg::ArgPack(vec![])]))),
                        ]
                    }
                }
                Err => {
                    // Ends with a prefix that is not a name or template.
                    b"NS_E..." => Error::UnexpectedText,
                    b"NS_DttrEE..." => Error::UnexpectedText,

                    b"zzz" => Error::UnexpectedText,
                    b"Nzzz" => Error::UnexpectedText,
                    b"NKzzz" => Error::UnexpectedText,
                    b"NKOzzz" => Error::UnexpectedText,
                    b"NKO3abczzz" => Error::UnexpectedText,
                    b"NKO3abc3abczzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                    b"N" => Error::UnexpectedEnd,
                    b"NK" => Error::UnexpectedEnd,
                    b"NKO" => Error::UnexpectedEnd,
                    b"NKO3abc" => Error::UnexpectedText,
                    b"NKO3abc3abc" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_prefix_handle() {
        // <prefix> ::= <unqualified-name>
        //          ::= <prefix> <unqualified-name>
        //          ::= <template-prefix> <template-args>
        //          ::= <template-param>
        //          ::= <decltype>
        //          ::= <prefix> <data-member-prefix>
        //          ::= <substitution>
        assert_parse!(PrefixHandle {
            with subs [
                Substitutable::Prefix(
                    Prefix::Unqualified(
                        UnqualifiedName::Operator(
                            OperatorName::New))),
            ] => {
                Ok => {
                    b"3foo..." => {
                        PrefixHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        }))))
                        ]
                    }
                    b"3abc3def..." => {
                        PrefixHandle::BackReference(2),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(1),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 5,
                                            end: 8,
                                        })))),
                        ]
                    }
                    b"3fooIJEE..." => {
                        PrefixHandle::BackReference(2),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::Template(PrefixHandle::BackReference(1),
                                                 TemplateArgs(vec![
                                                     TemplateArg::ArgPack(vec![]),
                                                 ])))
                        ]
                    }
                    b"T_..." => {
                        PrefixHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Prefix(Prefix::TemplateParam(TemplateParam(0))),
                        ]
                    }
                    b"DTtrE..." => {
                        PrefixHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Decltype(
                                    Decltype::Expression(Expression::Rethrow))),
                        ]
                    }
                    b"3abc3defM..." => {
                        PrefixHandle::BackReference(2),
                        b"...",
                        [
                            Substitutable::Prefix(
                                Prefix::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::DataMember(
                                    PrefixHandle::BackReference(1),
                                    DataMemberPrefix(
                                        SourceName(Identifier {
                                            start: 5,
                                            end: 8,
                                        })))),
                        ]
                    }
                    b"S_..." => {
                        PrefixHandle::BackReference(0),
                        b"...",
                        []
                    }
                    // The trailing E and <nested-name> case...
                    b"3abc3defE..." => {
                        PrefixHandle::BackReference(2),
                        b"E...",
                        [
                            Substitutable::Prefix(
                                Prefix::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        })))),
                            Substitutable::Prefix(
                                Prefix::Nested(
                                    PrefixHandle::BackReference(1),
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 5,
                                            end: 8,
                                        })))),
                        ]
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_type_handle() {
        assert_parse!(TypeHandle {
            with subs [
                Substitutable::Type(
                    Type::PointerTo(
                        TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Char)))),
            ] => {
                Ok => {
                    b"S_..." => {
                        TypeHandle::BackReference(0),
                        b"...",
                        []
                    }
                    b"c..." => {
                        TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Char)),
                        b"...",
                        []
                    }
                    b"FS_E..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::Function(FunctionType {
                                    cv_qualifiers: CvQualifiers {
                                        restrict: false,
                                        volatile: false,
                                        const_: false,
                                    },
                                    transaction_safe: false,
                                    extern_c: false,
                                    bare: BareFunctionType(vec![TypeHandle::BackReference(0)]),
                                    ref_qualifier: None,
                                })),
                        ]
                    }
                    b"A_S_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::Array(ArrayType::NoDimension(TypeHandle::BackReference(0)))),
                        ]
                    }
                    b"MS_S_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::PointerToMember(
                                    PointerToMemberType(TypeHandle::BackReference(0),
                                                        TypeHandle::BackReference(0)))),
                        ]
                    }
                    b"T_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::TemplateParam(TemplateParam(0))),
                        ]
                    }
                    b"T_IS_E..." => {
                        TypeHandle::BackReference(2),
                        b"...",
                        [
                            Substitutable::TemplateTemplateParam(
                                TemplateTemplateParam(TemplateParam(0))),
                            Substitutable::Type(
                                Type::TemplateTemplate(
                                    TemplateTemplateParamHandle::BackReference(1),
                                    TemplateArgs(vec![
                                        TemplateArg::Type(TypeHandle::BackReference(0))
                                    ]))),
                        ]
                    }
                    b"DTtrE..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::Decltype(Decltype::Expression(Expression::Rethrow))),
                        ]
                    }
                    b"KS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::Qualified(CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: true,
                            }, TypeHandle::BackReference(0)))
                        ]
                    }
                    b"PS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::PointerTo(TypeHandle::BackReference(0)))
                        ]
                    }
                    b"RS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::LvalueRef(TypeHandle::BackReference(0)))
                        ]
                    }
                    b"OS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::RvalueRef(TypeHandle::BackReference(0)))
                        ]
                    }
                    b"CS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::Complex(TypeHandle::BackReference(0)))
                        ]
                    }
                    b"GS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(Type::Imaginary(TypeHandle::BackReference(0)))
                        ]
                    }
                    b"U3abcS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::VendorExtension(
                                    SourceName(Identifier {
                                        start: 2,
                                        end: 5,
                                    }),
                                    None,
                                    TypeHandle::BackReference(0)))
                        ]
                    }
                    b"U3abcIS_ES_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::VendorExtension(
                                    SourceName(Identifier {
                                        start: 2,
                                        end: 5,
                                    }),
                                    Some(TemplateArgs(vec![
                                        TemplateArg::Type(TypeHandle::BackReference(0))
                                    ])),
                                    TypeHandle::BackReference(0)))
                        ]
                    }
                    b"DpS_..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::PackExpansion(TypeHandle::BackReference(0))),
                        ]
                    }
                    b"3abc..." => {
                        TypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::Type(
                                Type::ClassEnum(
                                    ClassEnumType::Named(
                                        Name::Unscoped(
                                            UnscopedName::Unqualified(
                                                UnqualifiedName::Source(
                                                    SourceName(Identifier {
                                                        start: 1,
                                                        end: 4,
                                                    })))))))
                        ]
                    }
                }
                Err => {
                    b"P" => Error::UnexpectedEnd,
                    b"R" => Error::UnexpectedEnd,
                    b"O" => Error::UnexpectedEnd,
                    b"C" => Error::UnexpectedEnd,
                    b"G" => Error::UnexpectedEnd,
                    b"Dp" => Error::UnexpectedEnd,
                    b"D" => Error::UnexpectedEnd,
                    b"P" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_function_type() {
        assert_parse!(FunctionType {
            with subs [
                Substitutable::Type(
                    Type::PointerTo(
                        TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Char)))),
            ] => {
                Ok => {
                    b"KDxFYS_RE..." => {
                        FunctionType {
                            cv_qualifiers: CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: true,
                            },
                            transaction_safe: true,
                            extern_c: true,
                            bare: BareFunctionType(vec![TypeHandle::BackReference(0)]),
                            ref_qualifier: Some(RefQualifier::LValueRef),
                        },
                        b"...",
                        []
                    }
                    b"DxFYS_RE..." => {
                        FunctionType {
                            cv_qualifiers: CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            transaction_safe: true,
                            extern_c: true,
                            bare: BareFunctionType(vec![TypeHandle::BackReference(0)]),
                            ref_qualifier: Some(RefQualifier::LValueRef),
                        },
                        b"...",
                        []
                    }
                    b"FYS_RE..." => {
                        FunctionType {
                            cv_qualifiers: CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            transaction_safe: false,
                            extern_c: true,
                            bare: BareFunctionType(vec![TypeHandle::BackReference(0)]),
                            ref_qualifier: Some(RefQualifier::LValueRef),
                        },
                        b"...",
                        []
                    }
                    b"FS_RE..." => {
                        FunctionType {
                            cv_qualifiers: CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            transaction_safe: false,
                            extern_c: false,
                            bare: BareFunctionType(vec![TypeHandle::BackReference(0)]),
                            ref_qualifier: Some(RefQualifier::LValueRef),
                        },
                        b"...",
                        []
                    }
                    b"FS_E..." => {
                        FunctionType {
                            cv_qualifiers: CvQualifiers {
                                restrict: false,
                                volatile: false,
                                const_: false,
                            },
                            transaction_safe: false,
                            extern_c: false,
                            bare: BareFunctionType(vec![TypeHandle::BackReference(0)]),
                            ref_qualifier: None,
                        },
                        b"...",
                        []
                    }
                }
                Err => {
                    b"DFYS_E" => Error::UnexpectedText,
                    b"KKFS_E" => Error::UnexpectedText,
                    b"FYS_..." => Error::UnexpectedText,
                    b"FYS_" => Error::UnexpectedEnd,
                    b"F" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_bare_function_type() {
        assert_parse!(BareFunctionType {
            with subs [
                Substitutable::Type(
                    Type::PointerTo(
                        TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Char)))),
            ] => {
                Ok => {
                    b"S_S_..." => {
                        BareFunctionType(vec![
                            TypeHandle::BackReference(0),
                            TypeHandle::BackReference(0),
                        ]),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_decltype() {
        assert_parse!(Decltype {
            Ok => {
                b"DTtrE..." => {
                    Decltype::Expression(Expression::Rethrow),
                    b"..."
                }
                b"DttrE..." => {
                    Decltype::IdExpression(Expression::Rethrow),
                    b"..."
                }
            }
            Err => {
                b"Dtrtz" => Error::UnexpectedText,
                b"DTrtz" => Error::UnexpectedText,
                b"Dz" => Error::UnexpectedText,
                b"Dtrt" => Error::UnexpectedText,
                b"DTrt" => Error::UnexpectedText,
                b"Dt" => Error::UnexpectedEnd,
                b"DT" => Error::UnexpectedEnd,
                b"D" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_class_enum_type() {
        assert_parse!(ClassEnumType {
            Ok => {
                b"3abc..." => {
                    ClassEnumType::Named(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 1,
                                        end: 4,
                                    }))))),
                    b"..."
                }
                b"Ts3abc..." => {
                    ClassEnumType::ElaboratedStruct(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 3,
                                        end: 6,
                                    }))))),
                    b"..."
                }
                b"Tu3abc..." => {
                    ClassEnumType::ElaboratedUnion(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 3,
                                        end: 6,
                                    }))))),
                    b"..."
                }
                b"Te3abc..." => {
                    ClassEnumType::ElaboratedEnum(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 3,
                                        end: 6,
                                    }))))),
                    b"..."
                }
            }
            Err => {
                b"zzz" => Error::UnexpectedText,
                b"Tzzz" => Error::UnexpectedText,
                b"T" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_array_type() {
        assert_parse!(ArrayType {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"A10_S_..." => {
                        ArrayType::DimensionNumber(10, TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"A10_Sb..." => {
                        ArrayType::DimensionNumber(10,
                                                   TypeHandle::WellKnown(
                                                       WellKnownComponent::StdString1)),
                        b"...",
                        []
                    }
                    b"Atr_S_..." => {
                        ArrayType::DimensionExpression(Expression::Rethrow,
                                                       TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"A_S_..." => {
                        ArrayType::NoDimension(TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"A10_" => Error::UnexpectedEnd,
                    b"A10" => Error::UnexpectedEnd,
                    b"A" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                    b"A10_..." => Error::UnexpectedText,
                    b"A10..." => Error::UnexpectedText,
                    b"A..." => Error::UnexpectedText,
                    b"..." => Error::UnexpectedText,
                }
            }
        });
    }

    #[test]
    fn parse_pointer_to_member_type() {
        assert_parse!(PointerToMemberType {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"MS_S_..." => {
                        PointerToMemberType(TypeHandle::BackReference(0),
                                            TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"MS_S" => Error::UnexpectedEnd,
                    b"MS_" => Error::UnexpectedEnd,
                    b"MS" => Error::UnexpectedEnd,
                    b"M" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                    b"MS_..." => Error::UnexpectedText,
                    b"M..." => Error::UnexpectedText,
                    b"..." => Error::UnexpectedText,
                }
            }
        });
    }

    #[test]
    fn parse_template_template_param_handle() {
        assert_parse!(TemplateTemplateParamHandle {
            with subs [
                Substitutable::TemplateTemplateParam(TemplateTemplateParam(TemplateParam(0)))
            ] => {
                Ok => {
                    b"S_..." => {
                        TemplateTemplateParamHandle::BackReference(0),
                        b"...",
                        []
                    }
                    b"T1_..." => {
                        TemplateTemplateParamHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::TemplateTemplateParam(TemplateTemplateParam(TemplateParam(2)))
                        ]
                    }
                }
                Err => {
                    b"S" => Error::UnexpectedText,
                    b"T" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                    b"S..." => Error::UnexpectedText,
                    b"T..." => Error::UnexpectedText,
                    b"..." => Error::UnexpectedText,
                }
            }
        });
    }

    #[test]
    fn parse_template_args() {
        assert_parse!(TemplateArgs {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"IS_E..." => {
                        TemplateArgs(vec![TemplateArg::Type(TypeHandle::BackReference(0))]),
                        b"...",
                        []
                    }
                    b"IS_S_S_S_E..." => {
                        TemplateArgs(vec![
                            TemplateArg::Type(TypeHandle::BackReference(0)),
                            TemplateArg::Type(TypeHandle::BackReference(0)),
                            TemplateArg::Type(TypeHandle::BackReference(0)),
                            TemplateArg::Type(TypeHandle::BackReference(0)),
                        ]),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"IE" => Error::UnexpectedText,
                    b"IS_" => Error::UnexpectedEnd,
                    b"I" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_template_arg() {
        assert_parse!(TemplateArg {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"S_..." => {
                        TemplateArg::Type(TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"XtrE..." => {
                        TemplateArg::Expression(Expression::Rethrow),
                        b"...",
                        []
                    }
                    b"LS_E..." => {
                        TemplateArg::SimpleExpression(
                            ExprPrimary::Literal(TypeHandle::BackReference(0), 3, 3)),
                        b"...",
                        []
                    }
                    b"JE..." => {
                        TemplateArg::ArgPack(vec![]),
                        b"...",
                        []
                    }
                    b"JS_XtrELS_EJEE..." => {
                        TemplateArg::ArgPack(vec![
                            TemplateArg::Type(TypeHandle::BackReference(0)),
                            TemplateArg::Expression(Expression::Rethrow),
                            TemplateArg::SimpleExpression(
                                ExprPrimary::Literal(TypeHandle::BackReference(0), 10, 10)),
                            TemplateArg::ArgPack(vec![]),
                        ]),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"..." => Error::UnexpectedText,
                    b"X..." => Error::UnexpectedText,
                    b"J..." => Error::UnexpectedText,
                    b"JS_..." => Error::UnexpectedText,
                    b"JS_" => Error::UnexpectedEnd,
                    b"X" => Error::UnexpectedEnd,
                    b"J" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_expression() {
        assert_parse!(Expression {
            with subs [
                Substitutable::Type(
                    Type::PointerTo(TypeHandle::Builtin(
                        BuiltinType::Standard(StandardBuiltinType::Int)))),
            ] => {
                Ok => {
                    b"psLS_1E..." => {
                        Expression::Unary(OperatorName::UnaryPlus,
                                          Box::new(Expression::Primary(
                                              ExprPrimary::Literal(
                                                  TypeHandle::BackReference(0),
                                                  5,
                                                  6)))),
                        b"...",
                        []
                    }
                    b"rsLS_1ELS_1E..." => {
                        Expression::Binary(OperatorName::Shr,
                                           Box::new(Expression::Primary(
                                               ExprPrimary::Literal(
                                                   TypeHandle::BackReference(0),
                                                   5,
                                                   6))),
                                           Box::new(Expression::Primary(
                                               ExprPrimary::Literal(
                                                   TypeHandle::BackReference(0),
                                                   10,
                                                   11)))),
                        b"...",
                        []
                    }
                    b"quLS_1ELS_2ELS_3E..." => {
                        Expression::Ternary(OperatorName::Question,
                                            Box::new(Expression::Primary(
                                                ExprPrimary::Literal(
                                                    TypeHandle::BackReference(0),
                                                    5,
                                                    6))),
                                            Box::new(Expression::Primary(
                                                ExprPrimary::Literal(
                                                    TypeHandle::BackReference(0),
                                                    10,
                                                    11))),
                                            Box::new(Expression::Primary(
                                                ExprPrimary::Literal(
                                                    TypeHandle::BackReference(0),
                                                    15,
                                                    16)))),
                        b"...",
                        []
                    }
                    b"pp_LS_1E..." => {
                        Expression::PrefixInc(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    6,
                                    7)))),
                        b"...",
                        []
                    }
                    b"mm_LS_1E..." => {
                        Expression::PrefixDec(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    6,
                                    7)))),
                        b"...",
                        []
                    }
                    b"clLS_1E..." => {
                        Expression::Call(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6))),
                            vec![]),
                        b"...",
                        []
                    }
                    //               ::= cv <type> <expression>                       # type (expression), conversion with one argument
                    b"cvS_LS_1E..." => {
                        Expression::ConversionOne(
                            TypeHandle::BackReference(0),
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"cvS__LS_1ELS_1EE..." => {
                        Expression::ConversionMany(
                            TypeHandle::BackReference(0),
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        8,
                                        9)),
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        13,
                                        14)),
                            ]),
                        b"...",
                        []
                    }
                    b"tlS_LS_1ELS_1EE..." => {
                        Expression::ConversionBraced(
                            TypeHandle::BackReference(0),
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        7,
                                        8)),
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        12,
                                        13)),
                            ]),
                        b"...",
                        []
                    }
                    b"ilLS_1EE..." => {
                        Expression::BracedInitList(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    b"gsnwLS_1E_S_E..." => {
                        Expression::GlobalNew(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        7,
                                        8))
                            ],
                            TypeHandle::BackReference(0),
                            None),
                        b"...",
                        []
                    }
                    b"nwLS_1E_S_E..." => {
                        Expression::New(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        5,
                                        6))
                            ],
                            TypeHandle::BackReference(0),
                            None),
                        b"...",
                        []
                    }
                    b"gsnwLS_1E_S_piE..." => {
                        Expression::GlobalNew(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        7,
                                        8))
                            ],
                            TypeHandle::BackReference(0),
                            Some(Initializer(vec![]))),
                        b"...",
                        []
                    }
                    b"nwLS_1E_S_piE..." => {
                        Expression::New(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        5,
                                        6))
                            ],
                            TypeHandle::BackReference(0),
                            Some(Initializer(vec![]))),
                        b"...",
                        []
                    }
                    b"gsnaLS_1E_S_E..." => {
                        Expression::GlobalNewArray(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        7,
                                        8))
                            ],
                            TypeHandle::BackReference(0),
                            None),
                        b"...",
                        []
                    }
                    b"naLS_1E_S_E..." => {
                        Expression::NewArray(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        5,
                                        6))
                            ],
                            TypeHandle::BackReference(0),
                            None),
                        b"...",
                        []
                    }
                    b"gsnaLS_1E_S_piE..." => {
                        Expression::GlobalNewArray(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        7,
                                        8))
                            ],
                            TypeHandle::BackReference(0),
                            Some(Initializer(vec![]))),
                        b"...",
                        []
                    }
                    b"naLS_1E_S_piE..." => {
                        Expression::NewArray(
                            vec![
                                Expression::Primary(
                                    ExprPrimary::Literal(
                                        TypeHandle::BackReference(0),
                                        5,
                                        6))
                            ],
                            TypeHandle::BackReference(0),
                            Some(Initializer(vec![]))),
                        b"...",
                        []
                    }
                    b"gsdlLS_1E..." => {
                        Expression::GlobalDelete(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"dlLS_1E..." => {
                        Expression::Delete(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    //               ::= [gs] da <expression>                         # delete[] expression
                    b"gsdaLS_1E..." => {
                        Expression::GlobalDeleteArray(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"daLS_1E..." => {
                        Expression::DeleteArray(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    b"dcS_LS_1E..." => {
                        Expression::DynamicCast(
                            TypeHandle::BackReference(0),
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"scS_LS_1E..." => {
                        Expression::StaticCast(
                            TypeHandle::BackReference(0),
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"ccS_LS_1E..." => {
                        Expression::ConstCast(
                            TypeHandle::BackReference(0),
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"rcS_LS_1E..." => {
                        Expression::ReinterpretCast(
                            TypeHandle::BackReference(0),
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    7,
                                    8)))),
                        b"...",
                        []
                    }
                    b"tiS_..." => {
                        Expression::TypeidType(TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"teLS_1E..." => {
                        Expression::TypeidExpr(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    b"stS_..." => {
                        Expression::SizeofType(TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"szLS_1E..." => {
                        Expression::SizeofExpr(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    b"atS_..." => {
                        Expression::AlignofType(TypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"azLS_1E..." => {
                        Expression::AlignofExpr(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    b"nxLS_1E..." => {
                        Expression::Noexcept(
                            Box::new(Expression::Primary(
                                ExprPrimary::Literal(
                                    TypeHandle::BackReference(0),
                                    5,
                                    6)))),
                        b"...",
                        []
                    }
                    b"T_..." => {
                        Expression::TemplateParam(TemplateParam(0)),
                        b"...",
                        []
                    }
                    b"fp_..." => {
                        Expression::FunctionParam(FunctionParam(0, CvQualifiers::default(), None)),
                        b"...",
                        []
                    }
                    b"dtT_3abc..." => {
                        Expression::Member(
                            Box::new(Expression::TemplateParam(TemplateParam(0))),
                            UnresolvedName::Name(
                                BaseUnresolvedName::Name(
                                    SimpleId(
                                        SourceName(
                                            Identifier {
                                                start: 5,
                                                end: 8,
                                            }),
                                        None)))),
                        b"...",
                        []
                    }
                    b"ptT_3abc..." => {
                        Expression::DerefMember(
                            Box::new(Expression::TemplateParam(TemplateParam(0))),
                            UnresolvedName::Name(
                                BaseUnresolvedName::Name(
                                    SimpleId(
                                        SourceName(
                                            Identifier {
                                                start: 5,
                                                end: 8,
                                            }),
                                        None)))),
                        b"...",
                        []
                    }
                    //               ::= ds <expression> <expression>                 # expr.*expr
                    b"dsT_T_..." => {
                        Expression::PointerToMember(
                            Box::new(Expression::TemplateParam(TemplateParam(0))),
                            Box::new(Expression::TemplateParam(TemplateParam(0)))),
                        b"...",
                        []
                    }
                    b"sZT_..." => {
                        Expression::SizeofTemplatePack(TemplateParam(0)),
                        b"...",
                        []
                    }
                    b"sZfp_..." => {
                        Expression::SizeofFunctionPack(
                            FunctionParam(0, CvQualifiers::default(), None)),
                        b"...",
                        []
                    }
                    b"sPE..." => {
                        Expression::SizeofCapturedTemplatePack(vec![]),
                        b"...",
                        []
                    }
                    b"spT_..." => {
                        Expression::PackExpansion(
                            Box::new(Expression::TemplateParam(TemplateParam(0)))),
                        b"...",
                        []
                    }
                    b"twT_..." => {
                        Expression::Throw(Box::new(Expression::TemplateParam(TemplateParam(0)))),
                        b"...",
                        []
                    }
                    b"tr..." => {
                        Expression::Rethrow,
                        b"...",
                        []
                    }
                    b"3abc..." => {
                        Expression::UnresolvedName(
                            UnresolvedName::Name(
                                BaseUnresolvedName::Name(
                                    SimpleId(
                                        SourceName(Identifier {
                                            start: 1,
                                            end: 4,
                                        }),
                                        None)))),
                        b"...",
                        []
                    }
                    b"L_Z3abcE..." => {
                        Expression::Primary(
                            ExprPrimary::External(
                                MangledName::Encoding(
                                    Encoding::Data(
                                        Name::Unscoped(
                                            UnscopedName::Unqualified(
                                                UnqualifiedName::Source(
                                                    SourceName(Identifier {
                                                        start: 4,
                                                        end: 7,
                                                    })))))))),
                        b"...",
                        []
                    }
                }
                Err => {
                }
            }
        });
    }

    #[test]
    fn parse_unresolved_name() {
        assert_parse!(UnresolvedName {
            with subs [
                Substitutable::UnresolvedType(
                    UnresolvedType::Decltype(Decltype::Expression(Expression::Rethrow))),
            ] => {
                Ok => {
                    b"gs3abc..." => {
                        UnresolvedName::Global(BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                            start: 3,
                            end: 6,
                        }), None))),
                        b"...",
                        []
                    }
                    b"3abc..." => {
                        UnresolvedName::Name(BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), None))),
                        b"...",
                        []
                    }
                    b"srS_3abc..." => {
                        UnresolvedName::Nested1(UnresolvedTypeHandle::BackReference(0),
                                                vec![],
                                                BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                                                    start: 5,
                                                    end: 8,
                                                }), None))),
                        b"...",
                        []
                    }
                    b"srNS_3abc3abcE3abc..." => {
                        UnresolvedName::Nested1(
                            UnresolvedTypeHandle::BackReference(0),
                            vec![
                                UnresolvedQualifierLevel(SimpleId(SourceName(Identifier {
                                    start: 6,
                                    end: 9,
                                }), None)),
                                UnresolvedQualifierLevel(SimpleId(SourceName(Identifier {
                                    start: 10,
                                    end: 13,
                                }), None)),
                            ],
                            BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                                start: 15,
                                end: 18,
                            }), None))),
                        b"...",
                        []
                    }
                    b"gssr3abcE3abc..." => {
                        UnresolvedName::GlobalNested2(
                            vec![
                                UnresolvedQualifierLevel(SimpleId(SourceName(Identifier {
                                    start: 5,
                                    end: 8,
                                }), None)),
                            ],
                            BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                                start: 10,
                                end: 13,
                            }), None))),
                        b"...",
                        []
                    }
                    b"sr3abcE3abc..." => {
                        UnresolvedName::Nested2(
                            vec![
                                UnresolvedQualifierLevel(SimpleId(SourceName(Identifier {
                                    start: 3,
                                    end: 6,
                                }), None)),
                            ],
                            BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                                start: 8,
                                end: 11,
                            }), None))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzzzzz" => Error::UnexpectedText,
                    b"gszzz" => Error::UnexpectedText,
                    b"gssrzzz" => Error::UnexpectedText,
                    b"srNzzz" => Error::UnexpectedText,
                    b"srzzz" => Error::UnexpectedText,
                    b"srN3abczzzz" => Error::UnexpectedText,
                    b"srN3abcE" => Error::UnexpectedText,
                    b"srN3abc" => Error::UnexpectedText,
                    b"srN" => Error::UnexpectedEnd,
                    b"sr" => Error::UnexpectedEnd,
                    b"gssr" => Error::UnexpectedEnd,
                    b"gs" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_unresolved_type_handle() {
        assert_parse!(UnresolvedTypeHandle {
            with subs [
                Substitutable::UnresolvedType(
                    UnresolvedType::Decltype(Decltype::Expression(Expression::Rethrow))),
            ] => {
                Ok => {
                    b"S_..." => {
                        UnresolvedTypeHandle::BackReference(0),
                        b"...",
                        []
                    }
                    b"T_..." => {
                        UnresolvedTypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::UnresolvedType(
                                UnresolvedType::Template(TemplateParam(0), None)),
                        ]
                    }
                    b"T_IS_E..." => {
                        UnresolvedTypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::UnresolvedType(
                                UnresolvedType::Template(TemplateParam(0), Some(TemplateArgs(vec![
                                    TemplateArg::Type(TypeHandle::BackReference(0))
                                ])))),
                        ]
                    }
                    b"DTtrE..." => {
                        UnresolvedTypeHandle::BackReference(1),
                        b"...",
                        [
                            Substitutable::UnresolvedType(
                                UnresolvedType::Decltype(Decltype::Expression(Expression::Rethrow)))
                        ]

                    }
                }
                Err => {
                    b"zzzzzzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_unresolved_qualifier_level() {
        assert_parse!(UnresolvedQualifierLevel {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"3abc..." => {
                        UnresolvedQualifierLevel(SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), None)),
                        b"...",
                        []
                    }
                    b"3abcIS_E..." => {
                        UnresolvedQualifierLevel(SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), Some(TemplateArgs(vec![
                            TemplateArg::Type(TypeHandle::BackReference(0))
                        ])))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_simple_id() {
        assert_parse!(SimpleId {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"3abc..." => {
                        SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), None),
                        b"...",
                        []
                    }
                    b"3abcIS_E..." => {
                        SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), Some(TemplateArgs(vec![
                            TemplateArg::Type(TypeHandle::BackReference(0))
                        ]))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_base_unresolved_name() {
        assert_parse!(BaseUnresolvedName {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"3abc..." => {
                        BaseUnresolvedName::Name(SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), None)),
                        b"...",
                        []
                    }
                    b"onnw..." => {
                        BaseUnresolvedName::Operator(OperatorName::New, None),
                        b"...",
                        []
                    }
                    b"onnwIS_E..." => {
                        BaseUnresolvedName::Operator(OperatorName::New, Some(TemplateArgs(vec![
                            TemplateArg::Type(TypeHandle::BackReference(0))
                        ]))),
                        b"...",
                        []
                    }
                    b"dn3abc..." => {
                        BaseUnresolvedName::Destructor(DestructorName::Name(SimpleId(SourceName(Identifier {
                            start: 3,
                            end: 6,
                        }), None))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"ozzz" => Error::UnexpectedText,
                    b"dzzz" => Error::UnexpectedText,
                    b"dn" => Error::UnexpectedEnd,
                    b"on" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_destructor_name() {
        assert_parse!(DestructorName {
            with subs [
                Substitutable::UnresolvedType(
                    UnresolvedType::Decltype(Decltype::Expression(Expression::Rethrow))),
            ] => {
                Ok => {
                    b"S_..." => {
                        DestructorName::Unresolved(UnresolvedTypeHandle::BackReference(0)),
                        b"...",
                        []
                    }
                    b"3abc..." => {
                        DestructorName::Name(SimpleId(SourceName(Identifier {
                            start: 1,
                            end: 4,
                        }), None)),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_expr_primary() {
        assert_parse!(ExprPrimary {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"LS_12345E..." => {
                        ExprPrimary::Literal(TypeHandle::BackReference(0), 3, 8),
                        b"...",
                        []
                    }
                    b"LS_E..." => {
                        ExprPrimary::Literal(TypeHandle::BackReference(0), 3, 3),
                        b"...",
                        []
                    }
                    b"L_Z3abcE..." => {
                        ExprPrimary::External(
                            MangledName::Encoding(
                                Encoding::Data(
                                    Name::Unscoped(
                                        UnscopedName::Unqualified(
                                            UnqualifiedName::Source(
                                                SourceName(Identifier {
                                                    start: 4,
                                                    end: 7,
                                                }))))))),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"zzz" => Error::UnexpectedText,
                    b"LS_zzz" => Error::UnexpectedEnd,
                    b"LS_12345" => Error::UnexpectedEnd,
                    b"LS_" => Error::UnexpectedEnd,
                    b"L" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_initializer() {
        assert_parse!(Initializer {
            Ok => {
                b"piE..." => {
                    Initializer(vec![]),
                    b"..."
                }
                b"pitrtrtrE..." => {
                    Initializer(vec![
                        Expression::Rethrow,
                        Expression::Rethrow,
                        Expression::Rethrow,
                    ]),
                    b"..."
                }
            }
            Err => {
                b"pirtrtrt..." => Error::UnexpectedText,
                b"pi..." => Error::UnexpectedText,
                b"..." => Error::UnexpectedText,
                b"pirt" => Error::UnexpectedText,
                b"pi" => Error::UnexpectedEnd,
                b"p" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_local_name() {
        assert_parse!(LocalName {
            Ok => {
                b"Z3abcE3def_0..." => {
                    LocalName::Relative(
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 2,
                                            end: 5,
                                        })))))),
                        Some(Box::new(Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 7,
                                        end: 10,
                                    })))))),
                        Some(Discriminator(0))),
                    b"..."
                }
                b"Z3abcE3def..." => {
                    LocalName::Relative(
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 2,
                                            end: 5,
                                        })))))),
                        Some(Box::new(Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 7,
                                        end: 10,
                                    })))))),
                        None),
                    b"..."
                }
                b"Z3abcEs_0..." => {
                    LocalName::Relative(
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 2,
                                            end: 5,
                                        })))))),
                        None,
                        Some(Discriminator(0))),
                    b"..."
                }
                b"Z3abcEs..." => {
                    LocalName::Relative(
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 2,
                                            end: 5,
                                        })))))),
                        None,
                        None),
                    b"..."
                }
                b"Z3abcEd1_3abc..." => {
                    LocalName::Default(
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 2,
                                            end: 5,
                                        })))))),
                        Some(1),
                        Box::new(Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 10,
                                        end: 13,
                                    })))))),
                    b"..."
                }
                b"Z3abcEd_3abc..." => {
                    LocalName::Default(
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 2,
                                            end: 5,
                                        })))))),
                        None,
                        Box::new(Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 9,
                                        end: 12,
                                    })))))),
                    b"..."
                }
            }
            Err => {
                b"A" => Error::UnexpectedText,
                b"Z1a" => Error::UnexpectedEnd,
                b"Z1aE" => Error::UnexpectedEnd,
                b"Z" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_closure_type_name() {
        assert_parse!(ClosureTypeName {
            Ok => {
                b"UlvE_..." => {
                    ClosureTypeName(LambdaSig(vec![]), None),
                    b"..."
                }
                b"UlvE36_..." => {
                    ClosureTypeName(LambdaSig(vec![]), Some(36)),
                    b"..."
                }
            }
            Err => {
                b"UlvE36zzz" => Error::UnexpectedText,
                b"UlvEzzz" => Error::UnexpectedText,
                b"Ulvzzz" => Error::UnexpectedText,
                b"zzz" => Error::UnexpectedText,
                b"UlvE10" => Error::UnexpectedEnd,
                b"UlvE" => Error::UnexpectedEnd,
                b"Ulv" => Error::UnexpectedEnd,
                b"Ul" => Error::UnexpectedEnd,
                b"U" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_lambda_sig() {
        assert_parse!(LambdaSig {
            with subs [
                Substitutable::Type(
                    Type::PointerTo(
                        TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Bool))))
            ] => {
                Ok => {
                    b"v..." => {
                        LambdaSig(vec![]),
                        b"...",
                        []
                    }
                    b"S_S_S_..." => {
                        LambdaSig(vec![
                            TypeHandle::BackReference(0),
                            TypeHandle::BackReference(0),
                            TypeHandle::BackReference(0),
                        ]),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"..." => Error::UnexpectedText,
                    b"S" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_substitution() {
        assert_parse!(Substitution {
            with subs [
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow))),
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow))),
                Substitutable::Type(Type::Decltype(Decltype::Expression(Expression::Rethrow)))
            ] => {
                Ok => {
                    b"S_..." => {
                        Substitution::BackReference(0),
                        b"...",
                        []
                    }
                    b"S1_..." => {
                        Substitution::BackReference(2),
                        b"...",
                        []
                    }
                    b"St..." => {
                        Substitution::WellKnown(WellKnownComponent::Std),
                        b"...",
                        []
                    }
                    b"Sa..." => {
                        Substitution::WellKnown(WellKnownComponent::StdAllocator),
                        b"...",
                        []
                    }
                    b"Sb..." => {
                        Substitution::WellKnown(WellKnownComponent::StdString1),
                        b"...",
                        []
                    }
                    b"Ss..." => {
                        Substitution::WellKnown(WellKnownComponent::StdString2),
                        b"...",
                        []
                    }
                    b"Si..." => {
                        Substitution::WellKnown(WellKnownComponent::StdIstream),
                        b"...",
                        []
                    }
                    b"So..." => {
                        Substitution::WellKnown(WellKnownComponent::StdOstream),
                        b"...",
                        []
                    }
                    b"Sd..." => {
                        Substitution::WellKnown(WellKnownComponent::StdIostream),
                        b"...",
                        []
                    }
                }
                Err => {
                    b"S999_" => Error::BadBackReference,
                    b"Sz" => Error::UnexpectedText,
                    b"zzz" => Error::UnexpectedText,
                    b"S1" => Error::UnexpectedEnd,
                    b"S" => Error::UnexpectedEnd,
                    b"" => Error::UnexpectedEnd,
                }
            }
        });
    }

    #[test]
    fn parse_special_name() {
        assert_parse!(SpecialName {
            Ok => {
                b"TVi..." => {
                    SpecialName::VirtualTable(TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Int))),
                    b"..."
                }
                b"TTi..." => {
                    SpecialName::Vtt(TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Int))),
                    b"..."
                }
                b"TIi..." => {
                    SpecialName::Typeinfo(TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Int))),
                    b"..."
                }
                b"TSi..." => {
                    SpecialName::TypeinfoName(TypeHandle::Builtin(BuiltinType::Standard(StandardBuiltinType::Int))),
                    b"..."
                }
                b"Tv42_36_3abc..." => {
                    SpecialName::VirtualOverrideThunk(
                        CallOffset::Virtual(VOffset(42, 36)),
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 9,
                                            end: 12,
                                        }))))))),
                    b"..."
                }
                b"Tcv42_36_v42_36_3abc..." => {
                    SpecialName::VirtualOverrideThunkCovariant(
                        CallOffset::Virtual(VOffset(42, 36)),
                        CallOffset::Virtual(VOffset(42, 36)),
                        Box::new(Encoding::Data(
                            Name::Unscoped(
                                UnscopedName::Unqualified(
                                    UnqualifiedName::Source(
                                        SourceName(Identifier {
                                            start: 17,
                                            end: 20,
                                        }))))))),
                    b"..."
                }
                b"GV3abc..." => {
                    SpecialName::Guard(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 3,
                                        end: 6,
                                    }))))),
                    b"..."
                }
                b"GR3abc_..." => {
                    SpecialName::GuardTemporary(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 3,
                                        end: 6,
                                    })))),
                        0),
                    b"..."
                }
                b"GR3abc0_..." => {
                    SpecialName::GuardTemporary(
                        Name::Unscoped(
                            UnscopedName::Unqualified(
                                UnqualifiedName::Source(
                                    SourceName(Identifier {
                                        start: 3,
                                        end: 6,
                                    })))),
                        1),
                    b"..."
                }
            }
            Err => {
                b"TZ" => Error::UnexpectedText,
                b"GZ" => Error::UnexpectedText,
                b"GR3abcz" => Error::UnexpectedText,
                b"GR3abc0z" => Error::UnexpectedText,
                b"T" => Error::UnexpectedEnd,
                b"G" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
                b"GR3abc" => Error::UnexpectedEnd,
                b"GR3abc0" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_function_param() {
        assert_parse!(FunctionParam {
            Ok => {
                b"fpK_..." => {
                    FunctionParam(0,
                                  CvQualifiers {
                                      restrict: false,
                                      volatile: false,
                                      const_: true,
                                  },
                                  None),
                    b"..."
                }
                b"fL1pK_..." => {
                    FunctionParam(1,
                                  CvQualifiers {
                                      restrict: false,
                                      volatile: false,
                                      const_: true,
                                  },
                                  None),
                    b"..."
                }
                b"fpK3_..." => {
                    FunctionParam(0,
                                  CvQualifiers {
                                      restrict: false,
                                      volatile: false,
                                      const_: true,
                                  },
                                  Some(3)),
                    b"..."
                }
                b"fL1pK4_..." => {
                    FunctionParam(1,
                                  CvQualifiers {
                                      restrict: false,
                                      volatile: false,
                                      const_: true,
                                  },
                                  Some(4)),
                    b"..."
                }
            }
            Err => {
                b"fz" => Error::UnexpectedText,
                b"fLp_" => Error::UnexpectedText,
                b"fpL_" => Error::UnexpectedText,
                b"fL1pK4z" => Error::UnexpectedText,
                b"fL1pK4" => Error::UnexpectedEnd,
                b"fL1p" => Error::UnexpectedEnd,
                b"fL1" => Error::UnexpectedEnd,
                b"fL" => Error::UnexpectedEnd,
                b"f" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_discriminator() {
        assert_parse!(Discriminator {
            Ok => {
                b"_0..." => {
                    Discriminator(0),
                    b"..."
                }
                b"_9..." => {
                    Discriminator(9),
                    b"..."
                }
                b"__99_..." => {
                    Discriminator(99),
                    b"..."
                }
            }
            Err => {
                b"_n1" => Error::UnexpectedText,
                b"__99..." => Error::UnexpectedText,
                b"__99" => Error::UnexpectedEnd,
                b"..." => Error::UnexpectedText,
            }
        });
    }

    #[test]
    fn parse_data_member_prefix() {
        assert_parse!(DataMemberPrefix {
            Ok => {
                b"3fooM..." => {
                    DataMemberPrefix(SourceName(Identifier {
                        start: 1,
                        end: 4,
                    })),
                    b"..."
                }
            }
            Err => {
                b"zzz" => Error::UnexpectedText,
                b"1" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_ref_qualifier() {
        assert_parse!(RefQualifier {
            Ok => {
                b"R..." => {
                    RefQualifier::LValueRef,
                    b"..."
                }
                b"O..." => {
                    RefQualifier::RValueRef,
                    b"..."
                }
            }
            Err => {
                b"..." => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_cv_qualifiers() {
        assert_parse!(CvQualifiers {
            Ok => {
                b"" => {
                    CvQualifiers { restrict: false, volatile: false, const_: false },
                    b""
                }
                b"..." => {
                    CvQualifiers { restrict: false, volatile: false, const_: false },
                    b"..."
                }
                b"r..." => {
                    CvQualifiers { restrict: true, volatile: false, const_: false },
                    b"..."
                }
                b"rV..." => {
                    CvQualifiers { restrict: true, volatile: true, const_: false },
                    b"..."
                }
                b"rVK..." => {
                    CvQualifiers { restrict: true, volatile: true, const_: true },
                    b"..."
                }
                b"V" => {
                    CvQualifiers { restrict: false, volatile: true, const_: false },
                    b""
                }
                b"VK" => {
                    CvQualifiers { restrict: false, volatile: true, const_: true },
                    b""
                }
                b"K..." => {
                    CvQualifiers { restrict: false, volatile: false, const_: true },
                    b"..."
                }
            }
            Err => {
                // None.
            }
        });
    }

    #[test]
    fn parse_builtin_type() {
        assert_parse!(BuiltinType {
            Ok => {
                b"c..." => {
                    BuiltinType::Standard(StandardBuiltinType::Char),
                    b"..."
                }
                b"c" => {
                    BuiltinType::Standard(StandardBuiltinType::Char),
                    b""
                }
                b"u3abc..." => {
                    BuiltinType::Extension(SourceName(Identifier {
                        start: 2,
                        end: 5,
                    })),
                    b"..."
                }
            }
            Err => {
                b"." => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_template_param() {
        assert_parse!(TemplateParam {
            Ok => {
                b"T_..." => {
                    TemplateParam(0),
                    b"..."
                }
                b"T3_..." => {
                    TemplateParam(4),
                    b"..."
                }
            }
            Err => {
                b"wtf" => Error::UnexpectedText,
                b"Twtf" => Error::UnexpectedText,
                b"T3wtf" => Error::UnexpectedText,
                b"T" => Error::UnexpectedEnd,
                b"T3" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_unscoped_name() {
        assert_parse!(UnscopedName {
            Ok => {
                b"St5hello..." => {
                    UnscopedName::Std(UnqualifiedName::Source(SourceName(Identifier {
                        start: 3,
                        end: 8,
                    }))),
                    b"..."
                }
                b"5hello..." => {
                    UnscopedName::Unqualified(UnqualifiedName::Source(SourceName(Identifier {
                        start: 1,
                        end: 6,
                    }))),
                    b"..."
                }
            }
            Err => {
                b"St..." => Error::UnexpectedText,
                b"..." => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_unqualified_name() {
        assert_parse!(UnqualifiedName {
            Ok => {
                b"qu.." => {
                    UnqualifiedName::Operator(OperatorName::Question),
                    b".."
                }
                b"C1.." => {
                    UnqualifiedName::CtorDtor(CtorDtorName::CompleteConstructor),
                    b".."
                }
                b"10abcdefghij..." => {
                    UnqualifiedName::Source(SourceName(Identifier {
                        start: 2,
                        end: 12,
                    })),
                    b"..."
                }
                b"Ut5_..." => {
                    UnqualifiedName::UnnamedType(UnnamedTypeName(Some(5))),
                    b"..."
                }
            }
            Err => {
                b"zzz" => Error::UnexpectedText,
                b"C" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_unnamed_type_name() {
        assert_parse!(UnnamedTypeName {
            Ok => {
                b"Ut_abc" => {
                    UnnamedTypeName(None),
                    b"abc"
                }
                b"Ut42_abc" => {
                    UnnamedTypeName(Some(42)),
                    b"abc"
                }
                b"Ut42_" => {
                    UnnamedTypeName(Some(42)),
                    b""
                }
            }
            Err => {
                b"ut_" => Error::UnexpectedText,
                b"u" => Error::UnexpectedEnd,
                b"Ut" => Error::UnexpectedEnd,
                b"Ut._" => Error::UnexpectedText,
                b"Ut42" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_identifier() {
        assert_parse!(Identifier {
            Ok => {
                b"1abc" => {
                    Identifier { start: 0, end: 4 },
                    b""
                }
                b"_Az1..." => {
                    Identifier { start: 0, end: 4 },
                    b"..."
                }
            }
            Err => {
                b"..." => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_source_name() {
        assert_parse!(SourceName {
            Ok => {
                b"1abc" => {
                    SourceName(Identifier { start: 1, end: 2 }),
                    b"bc"
                }
                b"10abcdefghijklm" => {
                    SourceName(Identifier { start: 2, end: 12 }),
                    b"klm"
                }
            }
            Err => {
                b"0abc" => Error::UnexpectedText,
                b"n1abc" => Error::UnexpectedText,
                b"10abcdef" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_number() {
        assert_parse!(Number {
            Ok => {
                b"n2n3" => {
                    -2,
                    b"n3"
                }
                b"12345abcdef" => {
                    12345,
                    b"abcdef"
                }
                b"0abcdef" => {
                    0,
                    b"abcdef"
                }
                b"42" => {
                    42,
                    b""
                }
            }
            Err => {
                b"001" => Error::UnexpectedText,
                b"wutang" => Error::UnexpectedText,
                b"n" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_call_offset() {
        assert_parse!(CallOffset {
            Ok => {
                b"hn42_..." => {
                    CallOffset::NonVirtual(NvOffset(-42)),
                    b"..."
                }
                b"vn42_36_..." => {
                    CallOffset::Virtual(VOffset(-42, 36)),
                    b"..."
                }
            }
            Err => {
                b"h1..." => Error::UnexpectedText,
                b"v1_1..." => Error::UnexpectedText,
                b"hh" => Error::UnexpectedText,
                b"vv" => Error::UnexpectedText,
                b"z" => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_v_offset() {
        assert_parse!(VOffset {
            Ok => {
                b"n2_n3abcdef" => {
                    VOffset(-2, -3),
                    b"abcdef"
                }
                b"12345_12345abcdef" => {
                    VOffset(12345, 12345),
                    b"abcdef"
                }
                b"0_0abcdef" => {
                    VOffset(0, 0),
                    b"abcdef"
                }
                b"42_n3" => {
                    VOffset(42, -3),
                    b""
                }
            }
            Err => {
                b"001" => Error::UnexpectedText,
                b"1_001" => Error::UnexpectedText,
                b"wutang" => Error::UnexpectedText,
                b"n_" => Error::UnexpectedText,
                b"1_n" => Error::UnexpectedEnd,
                b"1_" => Error::UnexpectedEnd,
                b"n" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_nv_offset() {
        assert_parse!(NvOffset {
            Ok => {
                b"n2n3" => {
                    NvOffset(-2),
                    b"n3"
                }
                b"12345abcdef" => {
                    NvOffset(12345),
                    b"abcdef"
                }
                b"0abcdef" => {
                    NvOffset(0),
                    b"abcdef"
                }
                b"42" => {
                    NvOffset(42),
                    b""
                }
            }
            Err => {
                b"001" => Error::UnexpectedText,
                b"wutang" => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_seq_id() {
        assert_parse!(SeqId {
            Ok => {
                b"1_" => {
                    SeqId(1),
                    b"_"
                }
                b"42" => {
                    SeqId(146),
                    b""
                }
                b"ABCabc" => {
                    SeqId(13368),
                    b"abc"
                }
            }
            Err => {
                b"abc" => Error::UnexpectedText,
                b"001" => Error::UnexpectedText,
                b"wutang" => Error::UnexpectedText,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_ctor_dtor_name() {
        assert_parse!(CtorDtorName {
            Ok => {
                b"D0" => {
                    CtorDtorName::DeletingDestructor,
                    b""
                }
                b"C101" => {
                    CtorDtorName::CompleteConstructor,
                    b"01"
                }
            }
            Err => {
                b"gayagaya" => Error::UnexpectedText,
                b"C" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    #[test]
    fn parse_operator_name() {
        assert_parse!(OperatorName {
            Ok => {
                b"qu" => {
                    OperatorName::Question,
                    b""
                }
                b"quokka" => {
                    OperatorName::Question,
                    b"okka"
                }
            }
            Err => {
                b"bu-buuuu" => Error::UnexpectedText,
                b"q" => Error::UnexpectedEnd,
                b"" => Error::UnexpectedEnd,
            }
        });
    }

    fn assert_demangle<I, S, D>(input: I, subs: S, thing: D, expected: &str)
        where I: AsRef<[u8]>,
              S: AsRef<[Substitutable]>,
              D: Demangle
    {
        let subs = SubstitutionTable::from_iter(subs.as_ref().iter().cloned());
        let mut buf: Vec<u8> = vec![];

        {
            let mut ctx = DemangleContext::new(&subs, input.as_ref(), &mut buf);
            thing.demangle(&mut ctx, None).unwrap();
        }

        if &buf[..] != expected.as_bytes() {
            panic!(r#"Given

input = "{}"

and subs = {:#?}

we expected "{}",
but found   "{}"."#,
                   String::from_utf8_lossy(input.as_ref()),
                   subs,
                   expected,
                   String::from_utf8_lossy(&buf[..]));
        }
    }

    #[test]
    fn demangle_operator_name() {
        assert_demangle("nw", [], OperatorName::New, "new");
    }

    #[test]
    fn demangle_standard_builtin_type() {
        assert_demangle("v", [], StandardBuiltinType::Void, "void");
    }

    #[test]
    fn demangle_well_known_component() {
        assert_demangle("Sa", [], WellKnownComponent::StdAllocator, "std::allocator");
    }
}
