use std::ffi::{OsStr, OsString};
use std::fmt;
use std::iter;
use std::ops::{Deref, DerefMut};
use std::path::Path;
use std::str;

use regex;
use regex::bytes::Regex;

use {Error, FILE_SEPARATORS, new_regex};
use pathutil::path_bytes;

/// Describes a matching strategy for a particular pattern.
///
/// This provides a way to more quickly determine whether a pattern matches
/// a particular file path in a way that scales with a large number of
/// patterns. For example, if many patterns are of the form `*.ext`, then it's
/// possible to test whether any of those patterns matches by looking up a
/// file path's extension in a hash table.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MatchStrategy {
    /// A pattern matches if and only if the entire file path matches this
    /// literal string.
    Literal(String),
    /// A pattern matches if and only if the file path's basename matches this
    /// literal string.
    BasenameLiteral(String),
    /// A pattern matches if and only if the file path's extension matches this
    /// literal string.
    Extension(OsString),
    /// A pattern matches if and only if this prefix literal is a prefix of the
    /// candidate file path.
    Prefix(String),
    /// A pattern matches if and only if this prefix literal is a prefix of the
    /// candidate file path.
    ///
    /// An exception: if `component` is true, then `suffix` must appear at the
    /// beginning of a file path or immediately following a `/`.
    Suffix {
        /// The actual suffix.
        suffix: String,
        /// Whether this must start at the beginning of a path component.
        component: bool,
    },
    /// A pattern matches only if the given extension matches the file path's
    /// extension. Note that this is a necessary but NOT sufficient criterion.
    /// Namely, if the extension matches, then a full regex search is still
    /// required.
    RequiredExtension(OsString),
    /// A regex needs to be used for matching.
    Regex,
}

impl MatchStrategy {
    /// Returns a matching strategy for the given pattern.
    pub fn new(pat: &Pattern) -> MatchStrategy {
        if let Some(lit) = pat.basename_literal() {
            MatchStrategy::BasenameLiteral(lit)
        } else if let Some(lit) = pat.literal() {
            MatchStrategy::Literal(lit)
        } else if let Some(ext) = pat.ext() {
            MatchStrategy::Extension(ext)
        } else if let Some(prefix) = pat.prefix() {
            MatchStrategy::Prefix(prefix)
        } else if let Some((suffix, component)) = pat.suffix() {
            MatchStrategy::Suffix { suffix: suffix, component: component }
        } else if let Some(ext) = pat.required_ext() {
            MatchStrategy::RequiredExtension(ext)
        } else {
            MatchStrategy::Regex
        }
    }
}

/// Pattern represents a successfully parsed shell glob pattern.
///
/// It cannot be used directly to match file paths, but it can be converted
/// to a regular expression string.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Pattern {
    glob: String,
    re: String,
    opts: PatternOptions,
    tokens: Tokens,
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        self.glob.fmt(f)
    }
}

/// A matcher for a single pattern.
#[derive(Clone, Debug)]
pub struct PatternMatcher {
    /// The underlying pattern.
    pat: Pattern,
    /// The pattern, as a compiled regex.
    re: Regex,
}

impl PatternMatcher {
    /// Tests whether the given path matches this pattern or not.
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        self.re.is_match(&*path_bytes(path.as_ref()))
    }
}

/// A strategic matcher for a single pattern.
#[cfg(test)]
#[derive(Clone, Debug)]
struct PatternStrategic {
    /// The match strategy to use.
    strategy: MatchStrategy,
    /// The underlying pattern.
    pat: Pattern,
    /// The pattern, as a compiled regex.
    re: Regex,
}

#[cfg(test)]
impl PatternStrategic {
    /// Tests whether the given path matches this pattern or not.
    pub fn is_match<P: AsRef<Path>>(&self, path: P) -> bool {
        use pathutil::file_name_ext;

        let cow_path = path_bytes(path.as_ref());
        let byte_path = &*cow_path;

        match self.strategy {
            MatchStrategy::Literal(ref lit) => lit.as_bytes() == byte_path,
            MatchStrategy::BasenameLiteral(ref lit) => {
                let lit = OsStr::new(lit);
                path.as_ref().file_name().map(|n| n == lit).unwrap_or(false)
            }
            MatchStrategy::Extension(ref ext) => {
                path.as_ref().file_name()
                    .and_then(file_name_ext)
                    .map(|got| got == ext)
                    .unwrap_or(false)
            }
            MatchStrategy::Prefix(ref pre) => {
                starts_with(pre.as_bytes(), byte_path)
            }
            MatchStrategy::Suffix { ref suffix, component } => {
                if component && byte_path == &suffix.as_bytes()[1..] {
                    return true;
                }
                ends_with(suffix.as_bytes(), byte_path)
            }
            MatchStrategy::RequiredExtension(ref ext) => {
                path.as_ref().file_name()
                    .and_then(file_name_ext)
                    .map(|got| got == ext && self.re.is_match(byte_path))
                    .unwrap_or(false)
            }
            MatchStrategy::Regex => self.re.is_match(byte_path),
        }
    }
}

/// A builder for a pattern.
///
/// This builder enables configuring the match semantics of a pattern. For
/// example, one can make matching case insensitive.
///
/// The lifetime `'a` refers to the lifetime of the pattern string.
#[derive(Clone, Debug)]
pub struct PatternBuilder<'a> {
    /// The glob pattern to compile.
    glob: &'a str,
    /// Options for the pattern.
    opts: PatternOptions,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct PatternOptions {
    /// Whether to match case insensitively.
    case_insensitive: bool,
    /// Whether to require a literal separator to match a separator in a file
    /// path. e.g., when enabled, `*` won't match `/`.
    literal_separator: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct Tokens(Vec<Token>);

impl Deref for Tokens {
    type Target = Vec<Token>;
    fn deref(&self) -> &Vec<Token> { &self.0 }
}

impl DerefMut for Tokens {
    fn deref_mut(&mut self) -> &mut Vec<Token> { &mut self.0 }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum Token {
    Literal(char),
    Any,
    ZeroOrMore,
    RecursivePrefix,
    RecursiveSuffix,
    RecursiveZeroOrMore,
    Class {
        negated: bool,
        ranges: Vec<(char, char)>,
    },
    Alternates(Vec<Tokens>),
}

impl Pattern {
    /// Builds a new pattern with default options.
    pub fn new(glob: &str) -> Result<Pattern, Error> {
        PatternBuilder::new(glob).build()
    }

    /// Returns a matcher for this pattern.
    pub fn compile_matcher(&self) -> PatternMatcher {
        let re = new_regex(&self.re)
            .expect("regex compilation shouldn't fail");
        PatternMatcher {
            pat: self.clone(),
            re: re,
        }
    }

    /// Returns a strategic matcher.
    ///
    /// This isn't exposed because it's not clear whether it's actually
    /// faster than just running a regex for a *single* pattern. If it
    /// is faster, then PatternMatcher should do it automatically.
    #[cfg(test)]
    fn compile_strategic_matcher(&self) -> PatternStrategic {
        let strategy = MatchStrategy::new(self);
        let re = new_regex(&self.re)
            .expect("regex compilation shouldn't fail");
        PatternStrategic {
            strategy: strategy,
            pat: self.clone(),
            re: re,
        }
    }

    /// Returns the original glob pattern used to build this pattern.
    pub fn glob(&self) -> &str {
        &self.glob
    }

    /// Returns the regular expression string for this glob.
    pub fn regex(&self) -> &str {
        &self.re
    }

    /// Returns true if and only if this pattern only inspects the basename
    /// of a path.
    pub fn is_only_basename(&self) -> bool {
        match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => {}
            _ => return false,
        }
        for t in &self.tokens[1..] {
            match *t {
                Token::Literal(c) if c == '/' || c == '\\' => return false,
                Token::RecursivePrefix
                | Token::RecursiveSuffix
                | Token::RecursiveZeroOrMore => return false,
                _ => {}
            }
        }
        true
    }

    /// Returns the pattern as a literal if and only if the pattern must match
    /// an entire path exactly.
    ///
    /// The basic format of these patterns is `{literal}`.
    pub fn literal(&self) -> Option<String> {
        if self.opts.case_insensitive {
            return None;
        }
        let mut lit = String::new();
        for t in &*self.tokens {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if lit.is_empty() {
            None
        } else {
            Some(lit)
        }
    }

    /// Returns an extension if this pattern matches a file path if and only
    /// if the file path has the extension returned.
    ///
    /// Note that this extension returned differs from the extension that
    /// std::path::Path::extension returns. Namely, this extension includes
    /// the '.'. Also, paths like `.rs` are considered to have an extension
    /// of `.rs`.
    pub fn ext(&self) -> Option<OsString> {
        if self.opts.case_insensitive {
            return None;
        }
        let start = match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => 1,
            Some(_) => 0,
            _ => return None,
        };
        match self.tokens.get(start) {
            Some(&Token::ZeroOrMore) => {
                // If there was no recursive prefix, then we only permit
                // `*` if `*` can match a `/`. For example, if `*` can't
                // match `/`, then `*.c` doesn't match `foo/bar.c`.
                if start == 0 && self.opts.literal_separator {
                    return None;
                }
            }
            _ => return None,
        }
        match self.tokens.get(start + 1) {
            Some(&Token::Literal('.')) => {}
            _ => return None,
        }
        let mut lit = OsStr::new(".").to_os_string();
        for t in self.tokens[start + 2..].iter() {
            match *t {
                Token::Literal('.') | Token::Literal('/') => return None,
                Token::Literal(c) => lit.push(c.to_string()),
                _ => return None,
            }
        }
        if lit.is_empty() {
            None
        } else {
            Some(lit)
        }
    }

    /// This is like `ext`, but returns an extension even if it isn't sufficent
    /// to imply a match. Namely, if an extension is returned, then it is
    /// necessary but not sufficient for a match.
    pub fn required_ext(&self) -> Option<OsString> {
        if self.opts.case_insensitive {
            return None;
        }
        // We don't care at all about the beginning of this pattern. All we
        // need to check for is if it ends with a literal of the form `.ext`.
        let mut ext: Vec<char> = vec![]; // built in reverse
        for t in self.tokens.iter().rev() {
            match *t {
                Token::Literal('/') => return None,
                Token::Literal(c) => {
                    ext.push(c);
                    if c == '.' {
                        break;
                    }
                }
                _ => return None,
            }
        }
        if ext.last() != Some(&'.') {
            None
        } else {
            ext.reverse();
            Some(OsString::from(ext.into_iter().collect::<String>()))
        }
    }

    /// Returns a literal prefix of this pattern if the entire pattern matches
    /// if the literal prefix matches.
    pub fn prefix(&self) -> Option<String> {
        if self.opts.case_insensitive {
            return None;
        }
        let end = match self.tokens.last() {
            Some(&Token::ZeroOrMore) => {
                if self.opts.literal_separator {
                    // If a trailing `*` can't match a `/`, then we can't
                    // assume a match of the prefix corresponds to a match
                    // of the overall pattern. e.g., `foo/*` with
                    // `literal_separator` enabled matches `foo/bar` but not
                    // `foo/bar/baz`, even though `foo/bar/baz` has a `foo/`
                    // literal prefix.
                    return None;
                }
                self.tokens.len() - 1
            }
            _ => self.tokens.len(),
        };
        let mut lit = String::new();
        for t in &self.tokens[0..end] {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if lit.is_empty() {
            None
        } else {
            Some(lit)
        }
    }

    /// Returns a literal suffix of this pattern if the entire pattern matches
    /// if the literal suffix matches.
    ///
    /// If a literal suffix is returned and it must match either the entire
    /// file path or be preceded by a `/`, then also return true. This happens
    /// with a pattern like `**/foo/bar`. Namely, this pattern matches
    /// `foo/bar` and `baz/foo/bar`, but not `foofoo/bar`. In this case, the
    /// suffix returned is `/foo/bar` (but should match the entire path
    /// `foo/bar`).
    ///
    /// When this returns true, the suffix literal is guaranteed to start with
    /// a `/`.
    pub fn suffix(&self) -> Option<(String, bool)> {
        if self.opts.case_insensitive {
            return None;
        }
        let mut lit = String::new();
        let (start, entire) = match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => {
                // We only care if this follows a path component if the next
                // token is a literal.
                if let Some(&Token::Literal(_)) = self.tokens.get(1) {
                    lit.push('/');
                    (1, true)
                } else {
                    (1, false)
                }
            }
            _ => (0, false),
        };
        let start = match self.tokens.get(start) {
            Some(&Token::ZeroOrMore) => {
                // If literal_separator is enabled, then a `*` can't
                // necessarily match everything, so reporting a suffix match
                // as a match of the pattern would be a false positive.
                if self.opts.literal_separator {
                    return None;
                }
                start + 1
            }
            _ => start,
        };
        for t in &self.tokens[start..] {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        if lit.is_empty() || lit == "/" {
            None
        } else {
            Some((lit, entire))
        }
    }

    /// If this pattern only needs to inspect the basename of a file path,
    /// then the tokens corresponding to only the basename match are returned.
    ///
    /// For example, given a pattern of `**/*.foo`, only the tokens
    /// corresponding to `*.foo` are returned.
    ///
    /// Note that this will return None if any match of the basename tokens
    /// doesn't correspond to a match of the entire pattern. For example, the
    /// glob `foo` only matches when a file path has a basename of `foo`, but
    /// doesn't *always* match when a file path has a basename of `foo`. e.g.,
    /// `foo` doesn't match `abc/foo`.
    fn basename_tokens(&self) -> Option<&[Token]> {
        if self.opts.case_insensitive {
            return None;
        }
        let start = match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => 1,
            _ => {
                // With nothing to gobble up the parent portion of a path,
                // we can't assume that matching on only the basename is
                // correct.
                return None;
            }
        };
        if self.tokens[start..].is_empty() {
            return None;
        }
        for t in &self.tokens[start..] {
            match *t {
                Token::Literal('/') => return None,
                Token::Literal(_) => {} // OK
                Token::Any | Token::ZeroOrMore => {
                    if !self.opts.literal_separator {
                        // In this case, `*` and `?` can match a path
                        // separator, which means this could reach outside
                        // the basename.
                        return None;
                    }
                }
                Token::RecursivePrefix
                | Token::RecursiveSuffix
                | Token::RecursiveZeroOrMore => {
                    return None;
                }
                Token::Class{..} | Token::Alternates(..) => {
                    // We *could* be a little smarter here, but either one
                    // of these is going to prevent our literal optimizations
                    // anyway, so give up.
                    return None;
                }
            }
        }
        Some(&self.tokens[start..])
    }

    /// Returns the pattern as a literal if and only if the pattern exclusiely
    /// matches the basename of a file path *and* is a literal.
    ///
    /// The basic format of these patterns is `**/{literal}`, where `{literal}`
    /// does not contain a path separator.
    pub fn basename_literal(&self) -> Option<String> {
        self.base_literal()
    }

    /// Returns the pattern as a literal if and only if the pattern exclusiely
    /// matches the basename of a file path *and* is a literal.
    ///
    /// The basic format of these patterns is `**/{literal}`, where `{literal}`
    /// does not contain a path separator.
    pub fn base_literal(&self) -> Option<String> {
        let tokens = match self.basename_tokens() {
            None => return None,
            Some(tokens) => tokens,
        };
        let mut lit = String::new();
        for t in tokens {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        Some(lit)
    }

    /// Returns a literal prefix of this pattern if and only if the entire
    /// pattern matches if the literal prefix matches.
    pub fn literal_prefix(&self) -> Option<String> {
        match self.tokens.last() {
            Some(&Token::ZeroOrMore) => {}
            _ => return None,
        }
        let mut lit = String::new();
        for t in &self.tokens[0..self.tokens.len()-1] {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        Some(lit)
    }

    /// Returns a literal suffix of this pattern if and only if the entire
    /// pattern matches if the literal suffix matches.
    pub fn literal_suffix(&self) -> Option<String> {
        match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => {}
            _ => return None,
        }
        let start =
            match self.tokens.get(1) {
                Some(&Token::ZeroOrMore) => 2,
                _ => 1,
            };
        let mut lit = String::new();
        for t in &self.tokens[start..] {
            match *t {
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        Some(lit)
    }

    /// Returns a basename literal prefix of this pattern.
    pub fn base_literal_prefix(&self) -> Option<String> {
        match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => {}
            _ => return None,
        }
        match self.tokens.last() {
            Some(&Token::ZeroOrMore) => {}
            _ => return None,
        }
        let mut lit = String::new();
        for t in &self.tokens[1..self.tokens.len()-1] {
            match *t {
                Token::Literal(c) if c == '/' || c == '\\' => return None,
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        Some(lit)
    }

    /// Returns a basename literal suffix of this pattern.
    pub fn base_literal_suffix(&self) -> Option<String> {
        match self.tokens.get(0) {
            Some(&Token::RecursivePrefix) => {}
            _ => return None,
        }
        match self.tokens.get(1) {
            Some(&Token::ZeroOrMore) => {}
            _ => return None,
        }
        let mut lit = String::new();
        for t in &self.tokens[2..] {
            match *t {
                Token::Literal(c) if c == '/' || c == '\\' => return None,
                Token::Literal(c) => lit.push(c),
                _ => return None,
            }
        }
        Some(lit)
    }
}

impl<'a> PatternBuilder<'a> {
    /// Create a new builder for the pattern given.
    ///
    /// The pattern is not compiled until `build` is called.
    pub fn new(glob: &'a str) -> PatternBuilder<'a> {
        PatternBuilder {
            glob: glob,
            opts: PatternOptions::default(),
        }
    }

    /// Parses and builds the pattern.
    pub fn build(&self) -> Result<Pattern, Error> {
        let mut p = Parser {
            stack: vec![Tokens::default()],
            chars: self.glob.chars().peekable(),
            prev: None,
            cur: None,
        };
        try!(p.parse());
        if p.stack.is_empty() {
            Err(Error::UnopenedAlternates)
        } else if p.stack.len() > 1 {
            Err(Error::UnclosedAlternates)
        } else {
            let tokens = p.stack.pop().unwrap();
            Ok(Pattern {
                glob: self.glob.to_string(),
                re: tokens.to_regex_with(&self.opts),
                opts: self.opts,
                tokens: tokens,
            })
        }
    }

    /// Toggle whether the pattern matches case insensitively or not.
    ///
    /// This is disabled by default.
    pub fn case_insensitive(&mut self, yes: bool) -> &mut PatternBuilder<'a> {
        self.opts.case_insensitive = yes;
        self
    }

    /// Toggle whether a literal `/` is required to match a path separator.
    pub fn literal_separator(&mut self, yes: bool) -> &mut PatternBuilder<'a> {
        self.opts.literal_separator = yes;
        self
    }
}

impl Tokens {
    /// Convert this pattern to a string that is guaranteed to be a valid
    /// regular expression and will represent the matching semantics of this
    /// glob pattern and the options given.
    fn to_regex_with(&self, options: &PatternOptions) -> String {
        let mut re = String::new();
        re.push_str("(?-u)");
        if options.case_insensitive {
            re.push_str("(?i)");
        }
        re.push('^');
        // Special case. If the entire glob is just `**`, then it should match
        // everything.
        if self.len() == 1 && self[0] == Token::RecursivePrefix {
            re.push_str(".*");
            re.push('$');
            return re;
        }
        self.tokens_to_regex(options, &self, &mut re);
        re.push('$');
        re
    }


    fn tokens_to_regex(
        &self,
        options: &PatternOptions,
        tokens: &[Token],
        re: &mut String,
    ) {
        let seps = &*FILE_SEPARATORS;

        for tok in tokens {
            match *tok {
                Token::Literal(c) => {
                    re.push_str(&regex::quote(&c.to_string()));
                }
                Token::Any => {
                    if options.literal_separator {
                        re.push_str(&format!("[^{}]", seps));
                    } else {
                        re.push_str(".");
                    }
                }
                Token::ZeroOrMore => {
                    if options.literal_separator {
                        re.push_str(&format!("[^{}]*", seps));
                    } else {
                        re.push_str(".*");
                    }
                }
                Token::RecursivePrefix => {
                    re.push_str(&format!("(?:[{sep}]?|.*[{sep}])", sep=seps));
                }
                Token::RecursiveSuffix => {
                    re.push_str(&format!("(?:[{sep}]?|[{sep}].*)", sep=seps));
                }
                Token::RecursiveZeroOrMore => {
                    re.push_str(&format!("(?:[{sep}]|[{sep}].*[{sep}])",
                                         sep=seps));
                }
                Token::Class { negated, ref ranges } => {
                    re.push('[');
                    if negated {
                        re.push('^');
                    }
                    for r in ranges {
                        if r.0 == r.1 {
                            // Not strictly necessary, but nicer to look at.
                            re.push_str(&regex::quote(&r.0.to_string()));
                        } else {
                            re.push_str(&regex::quote(&r.0.to_string()));
                            re.push('-');
                            re.push_str(&regex::quote(&r.1.to_string()));
                        }
                    }
                    re.push(']');
                }
                Token::Alternates(ref patterns) => {
                    let mut parts = vec![];
                    for pat in patterns {
                        let mut altre = String::new();
                        self.tokens_to_regex(options, &pat, &mut altre);
                        parts.push(altre);
                    }
                    re.push_str(&parts.join("|"));
                }
            }
        }
    }
}

struct Parser<'a> {
    stack: Vec<Tokens>,
    chars: iter::Peekable<str::Chars<'a>>,
    prev: Option<char>,
    cur: Option<char>,
}

impl<'a> Parser<'a> {
    fn parse(&mut self) -> Result<(), Error> {
        while let Some(c) = self.bump() {
            match c {
                '?' => try!(self.push_token(Token::Any)),
                '*' => try!(self.parse_star()),
                '[' => try!(self.parse_class()),
                '{' => try!(self.push_alternate()),
                '}' => try!(self.pop_alternate()),
                ',' => try!(self.parse_comma()),
                c => try!(self.push_token(Token::Literal(c))),
            }
        }
        Ok(())
    }

    fn push_alternate(&mut self) -> Result<(), Error> {
        if self.stack.len() > 1 {
            return Err(Error::NestedAlternates);
        }
        Ok(self.stack.push(Tokens::default()))
    }

    fn pop_alternate(&mut self) -> Result<(), Error> {
        let mut alts = vec![];
        while self.stack.len() >= 2 {
            alts.push(self.stack.pop().unwrap());
        }
        self.push_token(Token::Alternates(alts))
    }

    fn push_token(&mut self, tok: Token) -> Result<(), Error> {
        match self.stack.last_mut() {
            None => Err(Error::UnopenedAlternates),
            Some(ref mut pat) => Ok(pat.push(tok)),
        }
    }

    fn pop_token(&mut self) -> Result<Token, Error> {
        match self.stack.last_mut() {
            None => Err(Error::UnopenedAlternates),
            Some(ref mut pat) => Ok(pat.pop().unwrap()),
        }
    }

    fn have_tokens(&self) -> Result<bool, Error> {
        match self.stack.last() {
            None => Err(Error::UnopenedAlternates),
            Some(ref pat) => Ok(!pat.is_empty()),
        }
    }

    fn parse_comma(&mut self) -> Result<(), Error> {
        // If we aren't inside a group alternation, then don't
        // treat commas specially. Otherwise, we need to start
        // a new alternate.
        if self.stack.len() <= 1 {
            self.push_token(Token::Literal(','))
        } else {
            Ok(self.stack.push(Tokens::default()))
        }
    }

    fn parse_star(&mut self) -> Result<(), Error> {
        let prev = self.prev;
        if self.chars.peek() != Some(&'*') {
            try!(self.push_token(Token::ZeroOrMore));
            return Ok(());
        }
        assert!(self.bump() == Some('*'));
        if !try!(self.have_tokens()) {
            try!(self.push_token(Token::RecursivePrefix));
            let next = self.bump();
            if !next.is_none() && next != Some('/') {
                return Err(Error::InvalidRecursive);
            }
            return Ok(());
        }
        try!(self.pop_token());
        if prev != Some('/') {
            if self.stack.len() <= 1
                || (prev != Some(',') && prev != Some('{')) {
                return Err(Error::InvalidRecursive);
            }
        }
        match self.chars.peek() {
            None => {
                assert!(self.bump().is_none());
                self.push_token(Token::RecursiveSuffix)
            }
            Some(&',') | Some(&'}') if self.stack.len() >= 2 => {
                self.push_token(Token::RecursiveSuffix)
            }
            Some(&'/') => {
                assert!(self.bump() == Some('/'));
                self.push_token(Token::RecursiveZeroOrMore)
            }
            _ => Err(Error::InvalidRecursive),
        }
    }

    fn parse_class(&mut self) -> Result<(), Error> {
        fn add_to_last_range(
            r: &mut (char, char),
            add: char,
        ) -> Result<(), Error> {
            r.1 = add;
            if r.1 < r.0 {
                Err(Error::InvalidRange(r.0, r.1))
            } else {
                Ok(())
            }
        }
        let mut negated = false;
        let mut ranges = vec![];
        if self.chars.peek() == Some(&'!') {
            assert!(self.bump() == Some('!'));
            negated = true;
        }
        let mut first = true;
        let mut in_range = false;
        loop {
            let c = match self.bump() {
                Some(c) => c,
                // The only way to successfully break this loop is to observe
                // a ']'.
                None => return Err(Error::UnclosedClass),
            };
            match c {
                ']' => {
                    if first {
                        ranges.push((']', ']'));
                    } else {
                        break;
                    }
                }
                '-' => {
                    if first {
                        ranges.push(('-', '-'));
                    } else if in_range {
                        // invariant: in_range is only set when there is
                        // already at least one character seen.
                        let r = ranges.last_mut().unwrap();
                        try!(add_to_last_range(r, '-'));
                        in_range = false;
                    } else {
                        assert!(!ranges.is_empty());
                        in_range = true;
                    }
                }
                c => {
                    if in_range {
                        // invariant: in_range is only set when there is
                        // already at least one character seen.
                        try!(add_to_last_range(ranges.last_mut().unwrap(), c));
                    } else {
                        ranges.push((c, c));
                    }
                    in_range = false;
                }
            }
            first = false;
        }
        if in_range {
            // Means that the last character in the class was a '-', so add
            // it as a literal.
            ranges.push(('-', '-'));
        }
        self.push_token(Token::Class {
            negated: negated,
            ranges: ranges,
        })
    }

    fn bump(&mut self) -> Option<char> {
        self.prev = self.cur;
        self.cur = self.chars.next();
        self.cur
    }
}

#[cfg(test)]
fn starts_with(needle: &[u8], haystack: &[u8]) -> bool {
    needle.len() <= haystack.len() && needle == &haystack[..needle.len()]
}

#[cfg(test)]
fn ends_with(needle: &[u8], haystack: &[u8]) -> bool {
    if needle.len() > haystack.len() {
        return false;
    }
    needle == &haystack[haystack.len() - needle.len()..]
}

#[cfg(test)]
mod tests {
    use std::ffi::{OsStr, OsString};

    use {SetBuilder, Error};
    use super::{Pattern, PatternBuilder, Token};
    use super::Token::*;

    #[derive(Clone, Copy, Debug, Default)]
    struct Options {
        casei: bool,
        litsep: bool,
    }

    macro_rules! syntax {
        ($name:ident, $pat:expr, $tokens:expr) => {
            #[test]
            fn $name() {
                let pat = Pattern::new($pat).unwrap();
                assert_eq!($tokens, pat.tokens.0);
            }
        }
    }

    macro_rules! syntaxerr {
        ($name:ident, $pat:expr, $err:expr) => {
            #[test]
            fn $name() {
                let err = Pattern::new($pat).unwrap_err();
                assert_eq!($err, err);
            }
        }
    }

    macro_rules! toregex {
        ($name:ident, $pat:expr, $re:expr) => {
            toregex!($name, $pat, $re, Options::default());
        };
        ($name:ident, $pat:expr, $re:expr, $options:expr) => {
            #[test]
            fn $name() {
                let pat = PatternBuilder::new($pat)
                    .case_insensitive($options.casei)
                    .literal_separator($options.litsep)
                    .build()
                    .unwrap();
                assert_eq!(format!("(?-u){}", $re), pat.regex());
            }
        };
    }

    macro_rules! matches {
        ($name:ident, $pat:expr, $path:expr) => {
            matches!($name, $pat, $path, Options::default());
        };
        ($name:ident, $pat:expr, $path:expr, $options:expr) => {
            #[test]
            fn $name() {
                let pat = PatternBuilder::new($pat)
                    .case_insensitive($options.casei)
                    .literal_separator($options.litsep)
                    .build()
                    .unwrap();
                let matcher = pat.compile_matcher();
                let strategic = pat.compile_strategic_matcher();
                let set = SetBuilder::new().add(pat).build().unwrap();
                assert!(matcher.is_match($path));
                assert!(strategic.is_match($path));
                assert!(set.is_match($path));
            }
        };
    }

    macro_rules! nmatches {
        ($name:ident, $pat:expr, $path:expr) => {
            nmatches!($name, $pat, $path, Options::default());
        };
        ($name:ident, $pat:expr, $path:expr, $options:expr) => {
            #[test]
            fn $name() {
                let pat = PatternBuilder::new($pat)
                    .case_insensitive($options.casei)
                    .literal_separator($options.litsep)
                    .build()
                    .unwrap();
                let matcher = pat.compile_matcher();
                let strategic = pat.compile_strategic_matcher();
                let set = SetBuilder::new().add(pat).build().unwrap();
                assert!(!matcher.is_match($path));
                assert!(!strategic.is_match($path));
                assert!(!set.is_match($path));
            }
        };
    }

    fn s(string: &str) -> String { string.to_string() }
    fn os(string: &str) -> OsString { OsStr::new(string).to_os_string() }

    fn class(s: char, e: char) -> Token {
        Class { negated: false, ranges: vec![(s, e)] }
    }

    fn classn(s: char, e: char) -> Token {
        Class { negated: true, ranges: vec![(s, e)] }
    }

    fn rclass(ranges: &[(char, char)]) -> Token {
        Class { negated: false, ranges: ranges.to_vec() }
    }

    fn rclassn(ranges: &[(char, char)]) -> Token {
        Class { negated: true, ranges: ranges.to_vec() }
    }

    syntax!(literal1, "a", vec![Literal('a')]);
    syntax!(literal2, "ab", vec![Literal('a'), Literal('b')]);
    syntax!(any1, "?", vec![Any]);
    syntax!(any2, "a?b", vec![Literal('a'), Any, Literal('b')]);
    syntax!(seq1, "*", vec![ZeroOrMore]);
    syntax!(seq2, "a*b", vec![Literal('a'), ZeroOrMore, Literal('b')]);
    syntax!(seq3, "*a*b*", vec![
        ZeroOrMore, Literal('a'), ZeroOrMore, Literal('b'), ZeroOrMore,
    ]);
    syntax!(rseq1, "**", vec![RecursivePrefix]);
    syntax!(rseq2, "**/", vec![RecursivePrefix]);
    syntax!(rseq3, "/**", vec![RecursiveSuffix]);
    syntax!(rseq4, "/**/", vec![RecursiveZeroOrMore]);
    syntax!(rseq5, "a/**/b", vec![
        Literal('a'), RecursiveZeroOrMore, Literal('b'),
    ]);
    syntax!(cls1, "[a]", vec![class('a', 'a')]);
    syntax!(cls2, "[!a]", vec![classn('a', 'a')]);
    syntax!(cls3, "[a-z]", vec![class('a', 'z')]);
    syntax!(cls4, "[!a-z]", vec![classn('a', 'z')]);
    syntax!(cls5, "[-]", vec![class('-', '-')]);
    syntax!(cls6, "[]]", vec![class(']', ']')]);
    syntax!(cls7, "[*]", vec![class('*', '*')]);
    syntax!(cls8, "[!!]", vec![classn('!', '!')]);
    syntax!(cls9, "[a-]", vec![rclass(&[('a', 'a'), ('-', '-')])]);
    syntax!(cls10, "[-a-z]", vec![rclass(&[('-', '-'), ('a', 'z')])]);
    syntax!(cls11, "[a-z-]", vec![rclass(&[('a', 'z'), ('-', '-')])]);
    syntax!(cls12, "[-a-z-]", vec![
        rclass(&[('-', '-'), ('a', 'z'), ('-', '-')]),
    ]);
    syntax!(cls13, "[]-z]", vec![class(']', 'z')]);
    syntax!(cls14, "[--z]", vec![class('-', 'z')]);
    syntax!(cls15, "[ --]", vec![class(' ', '-')]);
    syntax!(cls16, "[0-9a-z]", vec![rclass(&[('0', '9'), ('a', 'z')])]);
    syntax!(cls17, "[a-z0-9]", vec![rclass(&[('a', 'z'), ('0', '9')])]);
    syntax!(cls18, "[!0-9a-z]", vec![rclassn(&[('0', '9'), ('a', 'z')])]);
    syntax!(cls19, "[!a-z0-9]", vec![rclassn(&[('a', 'z'), ('0', '9')])]);

    syntaxerr!(err_rseq1, "a**", Error::InvalidRecursive);
    syntaxerr!(err_rseq2, "**a", Error::InvalidRecursive);
    syntaxerr!(err_rseq3, "a**b", Error::InvalidRecursive);
    syntaxerr!(err_rseq4, "***", Error::InvalidRecursive);
    syntaxerr!(err_rseq5, "/a**", Error::InvalidRecursive);
    syntaxerr!(err_rseq6, "/**a", Error::InvalidRecursive);
    syntaxerr!(err_rseq7, "/a**b", Error::InvalidRecursive);
    syntaxerr!(err_unclosed1, "[", Error::UnclosedClass);
    syntaxerr!(err_unclosed2, "[]", Error::UnclosedClass);
    syntaxerr!(err_unclosed3, "[!", Error::UnclosedClass);
    syntaxerr!(err_unclosed4, "[!]", Error::UnclosedClass);
    syntaxerr!(err_range1, "[z-a]", Error::InvalidRange('z', 'a'));
    syntaxerr!(err_range2, "[z--]", Error::InvalidRange('z', '-'));

    const CASEI: Options = Options {
        casei: true,
        litsep: false,
    };
    const SLASHLIT: Options = Options {
        casei: false,
        litsep: true,
    };

    toregex!(re_casei, "a", "(?i)^a$", &CASEI);

    toregex!(re_slash1, "?", r"^[^/\\]$", SLASHLIT);
    toregex!(re_slash2, "*", r"^[^/\\]*$", SLASHLIT);

    toregex!(re1, "a", "^a$");
    toregex!(re2, "?", "^.$");
    toregex!(re3, "*", "^.*$");
    toregex!(re4, "a?", "^a.$");
    toregex!(re5, "?a", "^.a$");
    toregex!(re6, "a*", "^a.*$");
    toregex!(re7, "*a", "^.*a$");
    toregex!(re8, "[*]", r"^[\*]$");
    toregex!(re9, "[+]", r"^[\+]$");
    toregex!(re10, "+", r"^\+$");
    toregex!(re11, "**", r"^.*$");

    matches!(match1, "a", "a");
    matches!(match2, "a*b", "a_b");
    matches!(match3, "a*b*c", "abc");
    matches!(match4, "a*b*c", "a_b_c");
    matches!(match5, "a*b*c", "a___b___c");
    matches!(match6, "abc*abc*abc", "abcabcabcabcabcabcabc");
    matches!(match7, "a*a*a*a*a*a*a*a*a", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    matches!(match8, "a*b[xyz]c*d", "abxcdbxcddd");
    matches!(match9, "*.rs", ".rs");

    matches!(matchrec1, "some/**/needle.txt", "some/needle.txt");
    matches!(matchrec2, "some/**/needle.txt", "some/one/needle.txt");
    matches!(matchrec3, "some/**/needle.txt", "some/one/two/needle.txt");
    matches!(matchrec4, "some/**/needle.txt", "some/other/needle.txt");
    matches!(matchrec5, "**", "abcde");
    matches!(matchrec6, "**", "");
    matches!(matchrec7, "**", ".asdf");
    matches!(matchrec8, "**", "/x/.asdf");
    matches!(matchrec9, "some/**/**/needle.txt", "some/needle.txt");
    matches!(matchrec10, "some/**/**/needle.txt", "some/one/needle.txt");
    matches!(matchrec11, "some/**/**/needle.txt", "some/one/two/needle.txt");
    matches!(matchrec12, "some/**/**/needle.txt", "some/other/needle.txt");
    matches!(matchrec13, "**/test", "one/two/test");
    matches!(matchrec14, "**/test", "one/test");
    matches!(matchrec15, "**/test", "test");
    matches!(matchrec16, "/**/test", "/one/two/test");
    matches!(matchrec17, "/**/test", "/one/test");
    matches!(matchrec18, "/**/test", "/test");
    matches!(matchrec19, "**/.*", ".abc");
    matches!(matchrec20, "**/.*", "abc/.abc");
    matches!(matchrec21, ".*/**", ".abc");
    matches!(matchrec22, ".*/**", ".abc/abc");
    matches!(matchrec23, "foo/**", "foo");
    matches!(matchrec24, "**/foo/bar", "foo/bar");

    matches!(matchrange1, "a[0-9]b", "a0b");
    matches!(matchrange2, "a[0-9]b", "a9b");
    matches!(matchrange3, "a[!0-9]b", "a_b");
    matches!(matchrange4, "[a-z123]", "1");
    matches!(matchrange5, "[1a-z23]", "1");
    matches!(matchrange6, "[123a-z]", "1");
    matches!(matchrange7, "[abc-]", "-");
    matches!(matchrange8, "[-abc]", "-");
    matches!(matchrange9, "[-a-c]", "b");
    matches!(matchrange10, "[a-c-]", "b");
    matches!(matchrange11, "[-]", "-");

    matches!(matchpat1, "*hello.txt", "hello.txt");
    matches!(matchpat2, "*hello.txt", "gareth_says_hello.txt");
    matches!(matchpat3, "*hello.txt", "some/path/to/hello.txt");
    matches!(matchpat4, "*hello.txt", "some\\path\\to\\hello.txt");
    matches!(matchpat5, "*hello.txt", "/an/absolute/path/to/hello.txt");
    matches!(matchpat6, "*some/path/to/hello.txt", "some/path/to/hello.txt");
    matches!(matchpat7, "*some/path/to/hello.txt",
             "a/bigger/some/path/to/hello.txt");

    matches!(matchescape, "_[[]_[]]_[?]_[*]_!_", "_[_]_?_*_!_");

    matches!(matchcasei1, "aBcDeFg", "aBcDeFg", CASEI);
    matches!(matchcasei2, "aBcDeFg", "abcdefg", CASEI);
    matches!(matchcasei3, "aBcDeFg", "ABCDEFG", CASEI);
    matches!(matchcasei4, "aBcDeFg", "AbCdEfG", CASEI);

    matches!(matchalt1, "a,b", "a,b");
    matches!(matchalt2, ",", ",");
    matches!(matchalt3, "{a,b}", "a");
    matches!(matchalt4, "{a,b}", "b");
    matches!(matchalt5, "{**/src/**,foo}", "abc/src/bar");
    matches!(matchalt6, "{**/src/**,foo}", "foo");
    matches!(matchalt7, "{[}],foo}", "}");
    matches!(matchalt8, "{foo}", "foo");
    matches!(matchalt9, "{}", "");
    matches!(matchalt10, "{,}", "");
    matches!(matchalt11, "{*.foo,*.bar,*.wat}", "test.foo");
    matches!(matchalt12, "{*.foo,*.bar,*.wat}", "test.bar");
    matches!(matchalt13, "{*.foo,*.bar,*.wat}", "test.wat");

    matches!(matchslash1, "abc/def", "abc/def", SLASHLIT);
    nmatches!(matchslash2, "abc?def", "abc/def", SLASHLIT);
    nmatches!(matchslash2_win, "abc?def", "abc\\def", SLASHLIT);
    nmatches!(matchslash3, "abc*def", "abc/def", SLASHLIT);
    matches!(matchslash4, "abc[/]def", "abc/def", SLASHLIT); // differs

    nmatches!(matchnot1, "a*b*c", "abcd");
    nmatches!(matchnot2, "abc*abc*abc", "abcabcabcabcabcabcabca");
    nmatches!(matchnot3, "some/**/needle.txt", "some/other/notthis.txt");
    nmatches!(matchnot4, "some/**/**/needle.txt", "some/other/notthis.txt");
    nmatches!(matchnot5, "/**/test", "test");
    nmatches!(matchnot6, "/**/test", "/one/notthis");
    nmatches!(matchnot7, "/**/test", "/notthis");
    nmatches!(matchnot8, "**/.*", "ab.c");
    nmatches!(matchnot9, "**/.*", "abc/ab.c");
    nmatches!(matchnot10, ".*/**", "a.bc");
    nmatches!(matchnot11, ".*/**", "abc/a.bc");
    nmatches!(matchnot12, "a[0-9]b", "a_b");
    nmatches!(matchnot13, "a[!0-9]b", "a0b");
    nmatches!(matchnot14, "a[!0-9]b", "a9b");
    nmatches!(matchnot15, "[!-]", "-");
    nmatches!(matchnot16, "*hello.txt", "hello.txt-and-then-some");
    nmatches!(matchnot17, "*hello.txt", "goodbye.txt");
    nmatches!(matchnot18, "*some/path/to/hello.txt",
              "some/path/to/hello.txt-and-then-some");
    nmatches!(matchnot19, "*some/path/to/hello.txt",
              "some/other/path/to/hello.txt");
    nmatches!(matchnot20, "a", "foo/a");
    nmatches!(matchnot21, "./foo", "foo");
    nmatches!(matchnot22, "**/foo", "foofoo");
    nmatches!(matchnot23, "**/foo/bar", "foofoo/bar");
    nmatches!(matchnot24, "/*.c", "mozilla-sha1/sha1.c");
    nmatches!(matchnot25, "*.c", "mozilla-sha1/sha1.c", SLASHLIT);
    nmatches!(matchnot26, "**/m4/ltoptions.m4",
              "csharp/src/packages/repositories.config", SLASHLIT);

    macro_rules! extract {
        ($which:ident, $name:ident, $pat:expr, $expect:expr) => {
            extract!($which, $name, $pat, $expect, Options::default());
        };
        ($which:ident, $name:ident, $pat:expr, $expect:expr, $opts:expr) => {
            #[test]
            fn $name() {
                let pat = PatternBuilder::new($pat)
                    .case_insensitive($opts.casei)
                    .literal_separator($opts.litsep)
                    .build().unwrap();
                assert_eq!($expect, pat.$which());
            }
        };
    }

    macro_rules! literal {
        ($($tt:tt)*) => { extract!(literal, $($tt)*); }
    }

    macro_rules! basetokens {
        ($($tt:tt)*) => { extract!(basename_tokens, $($tt)*); }
    }

    macro_rules! ext {
        ($($tt:tt)*) => { extract!(ext, $($tt)*); }
    }

    macro_rules! required_ext {
        ($($tt:tt)*) => { extract!(required_ext, $($tt)*); }
    }

    macro_rules! prefix {
        ($($tt:tt)*) => { extract!(prefix, $($tt)*); }
    }

    macro_rules! suffix {
        ($($tt:tt)*) => { extract!(suffix, $($tt)*); }
    }

    macro_rules! baseliteral {
        ($($tt:tt)*) => { extract!(basename_literal, $($tt)*); }
    }

    literal!(extract_lit1, "foo", Some(s("foo")));
    literal!(extract_lit2, "foo", None, CASEI);
    literal!(extract_lit3, "/foo", Some(s("/foo")));
    literal!(extract_lit4, "/foo/", Some(s("/foo/")));
    literal!(extract_lit5, "/foo/bar", Some(s("/foo/bar")));
    literal!(extract_lit6, "*.foo", None);
    literal!(extract_lit7, "foo/bar", Some(s("foo/bar")));
    literal!(extract_lit8, "**/foo/bar", None);

    basetokens!(extract_basetoks1, "**/foo", Some(&*vec![
        Literal('f'), Literal('o'), Literal('o'),
    ]));
    basetokens!(extract_basetoks2, "**/foo", None, CASEI);
    basetokens!(extract_basetoks3, "**/foo", Some(&*vec![
        Literal('f'), Literal('o'), Literal('o'),
    ]), SLASHLIT);
    basetokens!(extract_basetoks4, "*foo", None, SLASHLIT);
    basetokens!(extract_basetoks5, "*foo", None);
    basetokens!(extract_basetoks6, "**/fo*o", None);
    basetokens!(extract_basetoks7, "**/fo*o", Some(&*vec![
        Literal('f'), Literal('o'), ZeroOrMore, Literal('o'),
    ]), SLASHLIT);

    ext!(extract_ext1, "**/*.rs", Some(os(".rs")));
    ext!(extract_ext2, "**/*.rs.bak", None);
    ext!(extract_ext3, "*.rs", Some(os(".rs")));
    ext!(extract_ext4, "a*.rs", None);
    ext!(extract_ext5, "/*.c", None);
    ext!(extract_ext6, "*.c", None, SLASHLIT);
    ext!(extract_ext7, "*.c", Some(os(".c")));

    required_ext!(extract_req_ext1, "*.rs", Some(os(".rs")));
    required_ext!(extract_req_ext2, "/foo/bar/*.rs", Some(os(".rs")));
    required_ext!(extract_req_ext3, "/foo/bar/*.rs", Some(os(".rs")));
    required_ext!(extract_req_ext4, "/foo/bar/.rs", Some(os(".rs")));
    required_ext!(extract_req_ext5, ".rs", Some(os(".rs")));
    required_ext!(extract_req_ext6, "./rs", None);
    required_ext!(extract_req_ext7, "foo", None);
    required_ext!(extract_req_ext8, ".foo/", None);
    required_ext!(extract_req_ext9, "foo/", None);

    prefix!(extract_prefix1, "/foo", Some(s("/foo")));
    prefix!(extract_prefix2, "/foo/*", Some(s("/foo/")));
    prefix!(extract_prefix3, "**/foo", None);
    prefix!(extract_prefix4, "foo/**", None);

    suffix!(extract_suffix1, "**/foo/bar", Some((s("/foo/bar"), true)));
    suffix!(extract_suffix2, "*/foo/bar", Some((s("/foo/bar"), false)));
    suffix!(extract_suffix3, "*/foo/bar", None, SLASHLIT);
    suffix!(extract_suffix4, "foo/bar", Some((s("foo/bar"), false)));
    suffix!(extract_suffix5, "*.foo", Some((s(".foo"), false)));
    suffix!(extract_suffix6, "*.foo", None, SLASHLIT);
    suffix!(extract_suffix7, "**/*_test", Some((s("_test"), false)));

    baseliteral!(extract_baselit1, "**/foo", Some(s("foo")));
    baseliteral!(extract_baselit2, "foo", None);
    baseliteral!(extract_baselit3, "*foo", None);
    baseliteral!(extract_baselit4, "*/foo", None);
}