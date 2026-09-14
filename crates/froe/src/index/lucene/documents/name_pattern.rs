//! `isRegexp` property names: Oak's `IndexDefinition.NamePattern`.
//!
//! `docs/analysis/lucene-oak-documents.md` §2.4. A pattern is **not** a
//! regular expression matched against a property path. Oak splits the
//! pattern text at its last `/` into a parent path and a name expression,
//! compares the parent path for equality, and only then matches the name
//! expression against the property's own name — whole-string, through
//! `Matcher.matches` rather than `find`.
//!
//! # The subset
//!
//! froe carries no regular-expression engine and will not approximate
//! Java's, so this module implements a **bounded subset** and refuses
//! everything else by name: literals, `.`, `*`, `+`, `?`, `|`, grouping,
//! character classes with negation, ranges and escapes, and `^` and `$`
//! at the ends, where `Matcher.matches` makes them redundant anyway.
//!
//! That is enough for Oak's own catch-all `^[^\/]*$` and for the patterns
//! AEM's shipped definitions use, `jcr:content/.*` and `.*Tags` among
//! them. A pattern outside it — `\d`, `\w`, `{2,3}`, `(?i)`, `(?:`, a
//! back-reference, a look-ahead, a nested class — is
//! [`IndexError::UnsupportedNamePattern`], because a pattern froe read
//! *approximately* would be a field that silently stops being written.

use crate::index::IndexError;

/// Oak's own catch-all, `FulltextIndexConstants.REGEX_ALL_PROPS`, which
/// its constructor special-cases: for this one pattern, and only this
/// one, the parent is empty and the whole text is the name expression.
pub const ALL_PROPERTIES: &str = r"^[^\/]*$";

/// One `isRegexp` property name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NamePattern {
    /// The pattern text, as stored.
    text: String,
    /// The parent path the property's own parent must equal.
    parent_path: String,
    /// The name expression.
    expression: Alternation,
}

impl NamePattern {
    /// Parses one pattern.
    ///
    /// # Errors
    ///
    /// [`IndexError::UnsupportedNamePattern`] for a construct outside the
    /// subset, and for one Java's own parser would refuse.
    pub fn parse(definition_path: &str, text: &str) -> Result<Self, IndexError> {
        let (parent_path, name) = if text == ALL_PROPERTIES {
            (String::new(), text)
        } else {
            (parent_path_of(text), name_of(text))
        };
        let expression =
            Parser::parse(name).map_err(|reason| IndexError::UnsupportedNamePattern {
                definition_path: definition_path.to_owned(),
                pattern: text.to_owned(),
                reason,
            })?;
        Ok(Self {
            text: text.to_owned(),
            parent_path,
            expression,
        })
    }

    /// The pattern text, as stored.
    #[must_use]
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Whether the pattern applies to a property path.
    #[must_use]
    pub fn matches(&self, property_path: &str) -> bool {
        if self.parent_path != parent_path_of(property_path) {
            return false;
        }
        let name: Vec<char> = name_of(property_path).chars().collect();
        self.expression.matches_whole(&name)
    }
}

/// Oak's `PathUtils.getParentPath`: everything before the last `/`, with a
/// leading `/` yielding `/` and no `/` at all yielding the empty string.
fn parent_path_of(path: &str) -> String {
    match path.rfind('/') {
        Some(0) => "/".to_owned(),
        Some(at) => path[..at].to_owned(),
        None => String::new(),
    }
}

/// Oak's `PathUtils.getName`: everything after the last `/`.
fn name_of(path: &str) -> &str {
    match path.rfind('/') {
        Some(at) => &path[at + 1..],
        None => path,
    }
}

// ----------------------------------------------------------- the subset

/// `a|b|c`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Alternation(Vec<Sequence>);

/// One branch of an alternation.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Sequence(Vec<Repeat>);

/// One atom with its quantifier.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Repeat {
    atom: Atom,
    quantifier: Quantifier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Quantifier {
    /// No quantifier.
    One,
    /// `*`.
    ZeroOrMore,
    /// `+`.
    OneOrMore,
    /// `?`.
    ZeroOrOne,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Atom {
    /// A literal character.
    Literal(char),
    /// `.`, which matches any character — a name holds no line
    /// terminator, so Java's own exclusion of them never shows.
    Any,
    /// `[…]`.
    Class(CharacterClass),
    /// `(…)`.
    Group(Alternation),
}

/// `[abc]`, `[^abc]`, `[a-z]`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct CharacterClass {
    negated: bool,
    members: Vec<ClassMember>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ClassMember {
    Single(char),
    Range(char, char),
}

impl CharacterClass {
    fn contains(&self, character: char) -> bool {
        let held = self.members.iter().any(|member| match *member {
            ClassMember::Single(one) => one == character,
            ClassMember::Range(first, last) => (first..=last).contains(&character),
        });
        held != self.negated
    }
}

// ----------------------------------------------------------- the parser

struct Parser<'pattern> {
    units: &'pattern [char],
    at: usize,
}

impl Parser<'_> {
    fn parse(text: &str) -> Result<Alternation, String> {
        let units: Vec<char> = text.chars().collect();
        let mut parser = Parser {
            units: &units,
            at: 0,
        };
        let parsed = parser.alternation(true)?;
        if parser.at != units.len() {
            return Err(format!("an unmatched `)` at {}", parser.at));
        }
        Ok(parsed)
    }

    fn peek(&self) -> Option<char> {
        self.units.get(self.at).copied()
    }

    /// `branch ( '|' branch )*`.
    fn alternation(&mut self, outermost: bool) -> Result<Alternation, String> {
        let mut branches = vec![self.sequence(outermost)?];
        while self.peek() == Some('|') {
            self.at += 1;
            branches.push(self.sequence(outermost)?);
        }
        Ok(Alternation(branches))
    }

    /// A run of quantified atoms.
    fn sequence(&mut self, outermost: bool) -> Result<Sequence, String> {
        let mut items = Vec::new();
        loop {
            match self.peek() {
                None | Some('|' | ')') => break,
                Some('^') if outermost && self.at == 0 => {
                    // `Matcher.matches` anchors both ends already, so a `^`
                    // at the start and a `$` at the end are redundant.
                    self.at += 1;
                }
                Some('$') if outermost && self.at + 1 == self.units.len() => {
                    self.at += 1;
                }
                Some('^' | '$') => {
                    return Err("an anchor anywhere but at the ends".to_owned());
                }
                Some(_) => {
                    let atom = self.atom()?;
                    let quantifier = self.quantifier()?;
                    if quantifier != Quantifier::One && contains_quantifier(&atom) {
                        // A quantifier over a quantified group is where a
                        // backtracking matcher goes exponential, and no
                        // definition froe has seen needs one.
                        return Err("a quantifier over a quantified group".to_owned());
                    }
                    items.push(Repeat { atom, quantifier });
                }
            }
        }
        Ok(Sequence(items))
    }

    fn quantifier(&mut self) -> Result<Quantifier, String> {
        let quantifier = match self.peek() {
            Some('*') => Quantifier::ZeroOrMore,
            Some('+') => Quantifier::OneOrMore,
            Some('?') => Quantifier::ZeroOrOne,
            Some('{') => return Err("a counted quantifier `{…}`".to_owned()),
            _ => return Ok(Quantifier::One),
        };
        self.at += 1;
        match self.peek() {
            // `a**`, `a*?` and `a*+` are a second quantifier, a lazy one
            // and a possessive one; Java refuses the first and means
            // something froe does not implement by the others.
            Some('*' | '+' | '?') => Err("a doubled quantifier".to_owned()),
            _ => Ok(quantifier),
        }
    }

    fn atom(&mut self) -> Result<Atom, String> {
        match self.peek() {
            Some('(') => {
                self.at += 1;
                if self.peek() == Some('?') {
                    return Err("a group flag, a look-around or a non-capturing group".to_owned());
                }
                let inner = self.alternation(false)?;
                if self.peek() != Some(')') {
                    return Err("an unclosed `(`".to_owned());
                }
                self.at += 1;
                Ok(Atom::Group(inner))
            }
            Some('[') => {
                self.at += 1;
                Ok(Atom::Class(self.character_class()?))
            }
            Some('.') => {
                self.at += 1;
                Ok(Atom::Any)
            }
            Some('\\') => {
                self.at += 1;
                Ok(Atom::Literal(self.escape()?))
            }
            Some(']' | '}') => Err("an unmatched `]` or `}`".to_owned()),
            Some(character) => {
                self.at += 1;
                Ok(Atom::Literal(character))
            }
            None => Err("an atom was expected".to_owned()),
        }
    }

    /// An escape, which froe accepts only where it means the character
    /// itself.
    fn escape(&mut self) -> Result<char, String> {
        let Some(character) = self.peek() else {
            return Err("a trailing `\\`".to_owned());
        };
        self.at += 1;
        if character.is_ascii_alphanumeric() {
            // `\d`, `\w`, `\s`, `\b`, `\1`, `\Q…\E` and the rest: each
            // means something this subset does not carry.
            return Err(format!("the escape `\\{character}`"));
        }
        Ok(character)
    }

    fn character_class(&mut self) -> Result<CharacterClass, String> {
        let negated = self.peek() == Some('^');
        if negated {
            self.at += 1;
        }
        let mut members = Vec::new();
        loop {
            let character = match self.peek() {
                None => return Err("an unclosed `[`".to_owned()),
                Some(']') if !members.is_empty() => {
                    self.at += 1;
                    return Ok(CharacterClass { negated, members });
                }
                Some('[') => return Err("a nested character class".to_owned()),
                Some('&') => return Err("a class intersection".to_owned()),
                Some('\\') => {
                    self.at += 1;
                    self.escape()?
                }
                Some(character) => {
                    self.at += 1;
                    character
                }
            };
            // A `-` opens a range unless it is the last character before
            // the `]`, where Java takes it literally.
            if self.peek() == Some('-')
                && self.units.get(self.at + 1).is_some_and(|next| *next != ']')
            {
                self.at += 1;
                let last = match self.peek() {
                    Some('\\') => {
                        self.at += 1;
                        self.escape()?
                    }
                    Some(character) => {
                        self.at += 1;
                        character
                    }
                    None => return Err("an unclosed `[`".to_owned()),
                };
                if last < character {
                    return Err("a range whose end precedes its start".to_owned());
                }
                members.push(ClassMember::Range(character, last));
            } else {
                members.push(ClassMember::Single(character));
            }
        }
    }
}

/// Whether an atom holds a quantifier anywhere inside it.
fn contains_quantifier(atom: &Atom) -> bool {
    let Atom::Group(alternation) = atom else {
        return false;
    };
    alternation.0.iter().any(|sequence| {
        sequence
            .0
            .iter()
            .any(|repeat| repeat.quantifier != Quantifier::One || contains_quantifier(&repeat.atom))
    })
}

// ---------------------------------------------------------- the matcher

impl Alternation {
    /// Whether the whole input matches, which is `Matcher.matches`.
    fn matches_whole(&self, input: &[char]) -> bool {
        self.matches_from(input, 0, &mut |at| at == input.len())
    }

    /// Whether some branch matches at `at` and the rest of the pattern —
    /// the continuation — accepts where it ends.
    fn matches_from(&self, input: &[char], at: usize, rest: &mut dyn FnMut(usize) -> bool) -> bool {
        self.0
            .iter()
            .any(|sequence| sequence.matches_from(input, at, rest))
    }
}

impl Sequence {
    fn matches_from(&self, input: &[char], at: usize, rest: &mut dyn FnMut(usize) -> bool) -> bool {
        match self.0.split_first() {
            None => rest(at),
            Some((first, remainder)) => {
                let remainder = Sequence(remainder.to_vec());
                first.matches_from(input, at, &mut |next| {
                    remainder.matches_from(input, next, rest)
                })
            }
        }
    }
}

impl Repeat {
    fn matches_from(&self, input: &[char], at: usize, rest: &mut dyn FnMut(usize) -> bool) -> bool {
        match self.quantifier {
            Quantifier::One => self.atom.matches_from(input, at, rest),
            Quantifier::ZeroOrOne => self.atom.matches_from(input, at, rest) || rest(at),
            Quantifier::ZeroOrMore => self.repeat_from(input, at, rest),
            Quantifier::OneOrMore => self
                .atom
                .matches_from(input, at, &mut |next| self.repeat_from(input, next, rest)),
        }
    }

    /// Zero or more of the atom, greedily, with backtracking — which for a
    /// whole-string match answers the same question as Java's own greedy
    /// matcher.
    fn repeat_from(&self, input: &[char], at: usize, rest: &mut dyn FnMut(usize) -> bool) -> bool {
        if self.atom.matches_from(input, at, &mut |next| {
            // An atom that consumed nothing would loop forever.
            next > at && self.repeat_from(input, next, rest)
        }) {
            return true;
        }
        rest(at)
    }
}

impl Atom {
    fn matches_from(&self, input: &[char], at: usize, rest: &mut dyn FnMut(usize) -> bool) -> bool {
        match self {
            Atom::Group(alternation) => alternation.matches_from(input, at, rest),
            Atom::Literal(expected) => {
                input.get(at).is_some_and(|found| found == expected) && rest(at + 1)
            }
            Atom::Any => at < input.len() && rest(at + 1),
            Atom::Class(class) => {
                input.get(at).is_some_and(|found| class.contains(*found)) && rest(at + 1)
            }
        }
    }
}
