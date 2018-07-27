use super::syntax_definition::*;
use super::scope::*;
#[cfg(feature = "yaml-load")]
use super::super::LoadingError;

use std::collections::{HashMap, HashSet};
use std::path::Path;
#[cfg(feature = "yaml-load")]
use walkdir::WalkDir;
#[cfg(feature = "yaml-load")]
use std::io::Read;
use std::io::{self, BufRead, BufReader};
use std::fs::File;
use std::mem;

use std::sync::Mutex;
use onig::Regex;
use parsing::syntax_definition::ContextId;

/// A syntax set holds a bunch of syntaxes and manages
/// loading them and the crucial operation of *linking*.
///
/// Linking replaces the references between syntaxes with direct
/// pointers. See `link_syntaxes` for more.
///
/// Re-linking— linking, adding more unlinked syntaxes with `load_syntaxes`,
/// and then linking again—is allowed.
#[derive(Debug, Serialize, Deserialize)]
pub struct SyntaxSet {
    syntaxes: Vec<SyntaxReference>,
    contexts: Vec<Context>,
    /// Stores the syntax index for every path that was loaded
    path_syntaxes: Vec<(String, usize)>,

    #[serde(skip_serializing, skip_deserializing)]
    first_line_cache: Mutex<FirstLineCache>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SyntaxReference {
    pub name: String,
    pub file_extensions: Vec<String>,
    pub scope: Scope,
    pub first_line_match: Option<String>,
    pub hidden: bool,
    #[serde(serialize_with = "ordered_map")]
    pub variables: HashMap<String, String>,

    #[serde(serialize_with = "ordered_map")]
    pub(crate) contexts: HashMap<String, ContextId>,
}

#[derive(Clone)]
pub struct SyntaxSetBuilder {
    syntaxes: Vec<SyntaxDefinition>,
    path_syntaxes: Vec<(String, usize)>,
}

#[cfg(feature = "yaml-load")]
fn load_syntax_file(p: &Path,
                    lines_include_newline: bool)
                    -> Result<SyntaxDefinition, LoadingError> {
    let mut f = File::open(p)?;
    let mut s = String::new();
    f.read_to_string(&mut s)?;

    Ok(SyntaxDefinition::load_from_str(&s, lines_include_newline, p.file_stem().and_then(|x| x.to_str()))?)
}

impl Clone for SyntaxSet {
    fn clone(&self) -> SyntaxSet {
        SyntaxSet {
            syntaxes: self.syntaxes.clone(),
            contexts: self.contexts.clone(),
            path_syntaxes: self.path_syntaxes.clone(),
            // Will need to be re-initialized
            first_line_cache: Mutex::new(FirstLineCache::new()),
        }
    }
}

impl Default for SyntaxSet {
    fn default() -> Self {
        SyntaxSet {
            syntaxes: Vec::new(),
            contexts: Vec::new(),
            path_syntaxes: Vec::new(),
            first_line_cache: Mutex::new(FirstLineCache::new()),
        }
    }
}


impl SyntaxSet {
    pub fn new() -> SyntaxSet {
        SyntaxSet::default()
    }

    /// Convenience constructor calling `new` and then `load_syntaxes` on the resulting set
    /// defaults to lines given not including newline characters, see the
    /// `load_syntaxes` method docs for an explanation as to why this might not be the best.
    /// It also links all the syntaxes together, see `link_syntaxes` for what that means.
    #[cfg(feature = "yaml-load")]
    pub fn load_from_folder<P: AsRef<Path>>(folder: P) -> Result<SyntaxSet, LoadingError> {
        let mut builder = SyntaxSetBuilder::new();
        builder.load_syntaxes(folder, false)?;
        Ok(builder.build())
    }

    /// The list of syntaxes in the set
    pub fn syntaxes(&self) -> &[SyntaxReference] {
        &self.syntaxes[..]
    }

    // TODO: visibility
    pub fn get_syntax(&self, index: usize) -> &SyntaxReference {
        &self.syntaxes[index]
    }

    /// Finds a syntax by its default scope, for example `source.regexp` finds the regex syntax.
    /// This and all similar methods below do a linear search of syntaxes, this should be fast
    /// because there aren't many syntaxes, but don't think you can call it a bajillion times per second.
    pub fn find_syntax_by_scope(&self, scope: Scope) -> Option<&SyntaxReference> {
        self.syntaxes.iter().find(|&s| s.scope == scope)
    }

    pub fn find_syntax_by_name<'a>(&'a self, name: &str) -> Option<&'a SyntaxReference> {
        self.syntaxes.iter().find(|&s| name == &s.name)
    }

    pub fn find_syntax_by_extension<'a>(&'a self, extension: &str) -> Option<&'a SyntaxReference> {
        self.syntaxes.iter().find(|&s| s.file_extensions.iter().any(|e| e == extension))
    }

    // TODO: visibility
    pub fn find_syntax_index_by_scope(&self, scope: Scope) -> Option<usize> {
        self.syntaxes.iter().position(|s| s.scope == scope)
    }

    // TODO: visibility
    pub fn find_syntax_index_by_name<'a>(&'a self, name: &str) -> Option<usize> {
        self.syntaxes.iter().position(|s| name == &s.name)
    }

    /// Searches for a syntax first by extension and then by case-insensitive name
    /// useful for things like Github-flavoured-markdown code block highlighting where
    /// all you have to go on is a short token given by the user
    pub fn find_syntax_by_token<'a>(&'a self, s: &str) -> Option<&'a SyntaxReference> {
        {
            let ext_res = self.find_syntax_by_extension(s);
            if ext_res.is_some() {
                return ext_res;
            }
        }
        self.syntaxes.iter().find(|&syntax| syntax.name.eq_ignore_ascii_case(s))
    }

    /// Try to find the syntax for a file based on its first line.
    /// This uses regexes that come with some sublime syntax grammars
    /// for matching things like shebangs and mode lines like `-*- Mode: C -*-`
    pub fn find_syntax_by_first_line<'a>(&'a self, s: &str) -> Option<&'a SyntaxReference> {
        let mut cache = self.first_line_cache.lock().unwrap();
        cache.ensure_filled(self.syntaxes());
        for &(ref reg, i) in &cache.regexes {
            if reg.find(s).is_some() {
                return Some(&self.syntaxes[i]);
            }
        }
        None
    }

    /// Searches for a syntax by it's original file path when it was first loaded from disk
    /// primarily useful for syntax tests
    /// some may specify a Packages/PackageName/SyntaxName.sublime-syntax path
    /// others may just have SyntaxName.sublime-syntax
    /// this caters for these by matching the end of the path of the loaded syntax definition files
    // however, if a syntax name is provided without a folder, make sure we don't accidentally match the end of a different syntax definition's name - by checking a / comes before it or it is the full path
    pub fn find_syntax_by_path<'a>(&'a self, path: &str) -> Option<&'a SyntaxReference> {
        let mut slash_path = "/".to_string();
        slash_path.push_str(&path);
        return self.path_syntaxes.iter().find(|t| t.0.ends_with(&slash_path) || t.0 == path).map(|&(_,i)| &self.syntaxes[i]);
    }

    /// Convenience method that tries to find the syntax for a file path,
    /// first by extension/name and then by first line of the file if that doesn't work.
    /// May IO Error because it sometimes tries to read the first line of the file.
    ///
    /// # Examples
    /// When determining how to highlight a file, use this in combination with a fallback to plain text:
    ///
    /// ```
    /// use syntect::parsing::SyntaxSet;
    /// let ss = SyntaxSet::load_defaults_nonewlines();
    /// let syntax = ss.find_syntax_for_file("testdata/highlight_test.erb")
    ///     .unwrap() // for IO errors, you may want to use try!() or another plain text fallback
    ///     .unwrap_or_else(|| ss.find_syntax_plain_text());
    /// assert_eq!(syntax.name, "HTML (Rails)");
    /// ```
    pub fn find_syntax_for_file<P: AsRef<Path>>(&self,
                                                path_obj: P)
                                                -> io::Result<Option<&SyntaxReference>> {
        let path: &Path = path_obj.as_ref();
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let extension = path.extension().and_then(|x| x.to_str()).unwrap_or("");
        let ext_syntax = self.find_syntax_by_extension(file_name).or_else(
                            || self.find_syntax_by_extension(extension));
        let line_syntax = if ext_syntax.is_none() {
            let mut line = String::new();
            let f = File::open(path)?;
            let mut line_reader = BufReader::new(&f);
            line_reader.read_line(&mut line)?;
            self.find_syntax_by_first_line(&line)
        } else {
            None
        };
        let syntax = ext_syntax.or(line_syntax);
        Ok(syntax)
    }

    /// Finds a syntax for plain text, which usually has no highlighting rules.
    /// Good as a fallback when you can't find another syntax but you still want
    /// to use the same highlighting pipeline code.
    ///
    /// This syntax should always be present, if not this method will panic.
    /// If the way you load syntaxes doesn't create one, use `load_plain_text_syntax`.
    ///
    /// # Examples
    /// ```
    /// use syntect::parsing::SyntaxSetBuilder;
    /// let mut builder = SyntaxSetBuilder::new();
    /// builder.load_plain_text_syntax();
    /// let ss = builder.build();
    /// let syntax = ss.find_syntax_by_token("rs").unwrap_or_else(|| ss.find_syntax_plain_text());
    /// assert_eq!(syntax.name, "Plain Text");
    /// ```
    pub fn find_syntax_plain_text(&self) -> &SyntaxReference {
        self.find_syntax_by_name("Plain Text")
            .expect("All syntax sets ought to have a plain text syntax")
    }

    pub fn into_builder(self) -> SyntaxSetBuilder {
        let SyntaxSet { syntaxes, contexts, path_syntaxes, .. } = self;

        let mut context_map = HashMap::with_capacity(contexts.len());
        for (i, context) in contexts.into_iter().enumerate() {
            context_map.insert(i, context);
        }

        let mut builder_syntaxes = Vec::with_capacity(syntaxes.len());

        for syntax in syntaxes {
            let SyntaxReference {
                name,
                file_extensions,
                scope,
                first_line_match,
                hidden,
                variables,
                contexts,
            } = syntax;

            let mut builder_contexts = HashMap::with_capacity(contexts.len());
            for (name, context_id) in contexts {
                if let Some(context) = context_map.remove(&context_id.index()) {
                    builder_contexts.insert(name, context);
                }
            }

            let syntax_definition = SyntaxDefinition {
                name,
                file_extensions,
                scope,
                first_line_match,
                hidden,
                variables,
                contexts: builder_contexts,
            };
            builder_syntaxes.push(syntax_definition);
        }

        SyntaxSetBuilder {
            syntaxes: builder_syntaxes,
            path_syntaxes,
        }
    }

    pub(crate) fn get_context(&self, context_id: &ContextId) -> &Context {
        &self.contexts[context_id.index()]
    }
}


impl SyntaxSetBuilder {
    pub fn new() -> SyntaxSetBuilder {
        SyntaxSetBuilder {
            syntaxes: Vec::new(),
            path_syntaxes: Vec::new(),
        }
    }

    /// Add a syntax to the set.
    pub fn add_syntax(&mut self, syntax: SyntaxDefinition) {
        self.syntaxes.push(syntax);
    }

    /// Rarely useful method that loads in a syntax with no highlighting rules for plain text.
    /// Exists mainly for adding the plain text syntax to syntax set dumps, because for some
    /// reason the default Sublime plain text syntax is still in `.tmLanguage` format.
    #[cfg(feature = "yaml-load")]
    pub fn load_plain_text_syntax(&mut self) {
        let s = "---\nname: Plain Text\nfile_extensions: [txt]\nscope: text.plain\ncontexts: \
                 {main: []}";
        let syn = SyntaxDefinition::load_from_str(s, false, None).unwrap();
        self.syntaxes.push(syn);
    }
    /// Loads all the .sublime-syntax files in a folder into this syntax set.
    ///
    /// The `lines_include_newline` parameter is used to work around the fact that Sublime Text normally
    /// passes line strings including newline characters (`\n`) to its regex engine. This results in many
    /// syntaxes having regexes matching `\n`, which doesn't work if you don't pass in newlines.
    /// It is recommended that if you can you pass in lines with newlines if you can and pass `true` for this parameter.
    /// If that is inconvenient pass `false` and the loader will do some hacky find and replaces on the
    /// match regexes that seem to work for the default syntax set, but may not work for any other syntaxes.
    ///
    /// In the future I might include a "slow mode" that copies the lines passed in and appends a newline if there isn't one.
    /// but in the interest of performance currently this hacky fix will have to do.
    #[cfg(feature = "yaml-load")]
    pub fn load_syntaxes<P: AsRef<Path>>(&mut self,
                                         folder: P,
                                         lines_include_newline: bool)
                                         -> Result<(), LoadingError> {
        for entry in WalkDir::new(folder).sort_by(|a, b| a.file_name().cmp(b.file_name())) {
            let entry = entry.map_err(LoadingError::WalkDir)?;
            if entry.path().extension().map_or(false, |e| e == "sublime-syntax") {
                let syntax = load_syntax_file(entry.path(), lines_include_newline)?;
                if let Some(path_str) = entry.path().to_str() {
                    // Split the path up and rejoin with slashes so that syntaxes loaded on Windows
                    // can still be loaded the same way.
                    let path = Path::new(path_str);
                    let path_parts: Vec<_> = path.iter().map(|c| c.to_str().unwrap()).collect();
                    self.path_syntaxes.push((path_parts.join("/").to_string(), self.syntaxes.len()));
                }
                self.syntaxes.push(syntax);
            }
        }
        Ok(())
    }

    /// This links all the syntaxes in this set directly with pointers for performance purposes.
    /// It is necessary to do this before parsing anything with these syntaxes.
    /// However, it is not possible to serialize a syntax set that has been linked,
    /// which is why it isn't done by default, except by the load_from_folder constructor.
    /// This operation is idempotent, but takes time even on already linked syntax sets.
    pub fn build(self) -> SyntaxSet {
        let SyntaxSetBuilder { syntaxes: syntax_definitions, path_syntaxes } = self;

        let mut syntaxes = Vec::with_capacity(syntax_definitions.len());
        let mut all_contexts = Vec::new();

        for syntax_definition in syntax_definitions {
            let SyntaxDefinition {
                name,
                file_extensions,
                scope,
                first_line_match,
                hidden,
                variables,
                contexts,
            } = syntax_definition;

            let mut map = HashMap::new();

            let mut contexts: Vec<(String, Context)> = contexts.into_iter().collect();
            // Sort the values of the HashMap so that the contexts in the
            // resulting SyntaxSet have a deterministic order for serializing.
            contexts.sort_by(|(name_a, _), (name_b, _)| name_a.cmp(&name_b));
            for (name, context) in contexts {
                let index = all_contexts.len();
                map.insert(name, ContextId::new(index));
                all_contexts.push(context);
            }

            let syntax = SyntaxReference {
                name,
                file_extensions,
                scope,
                first_line_match,
                hidden,
                variables,
                contexts: map,
            };
            syntaxes.push(syntax);
        }

        for syntax in &syntaxes {
            let mut no_prototype = HashSet::new();
            let prototype = syntax.contexts.get("prototype");
            if let Some(prototype_id) = prototype {
                // TODO: We could do this after parsing YAML, instead of here?
                Self::recursively_mark_no_prototype(syntax, prototype_id.index(), &all_contexts, &mut no_prototype);
            }

            for context_id in syntax.contexts.values() {
                let mut context = &mut all_contexts[context_id.index()];
                if let Some(prototype_id) = prototype {
                    if context.meta_include_prototype && !no_prototype.contains(&context_id.index()) {
                        context.prototype = Some(prototype_id.clone());
                    }
                }
                Self::link_context(&mut context, syntax, &syntaxes);
            }
        }

        SyntaxSet {
            syntaxes,
            contexts: all_contexts,
            path_syntaxes,
            first_line_cache: Mutex::new(FirstLineCache::new()),
        }
    }

    /// Anything recursively included by the prototype shouldn't include the prototype.
    /// This marks them as such.
    fn recursively_mark_no_prototype(
        syntax: &SyntaxReference,
        context_id: usize,
        contexts: &[Context],
        no_prototype: &mut HashSet<usize>,
    ) {
        let first_time = no_prototype.insert(context_id);
        if !first_time {
            return;
        }

        for pattern in &contexts[context_id].patterns {
            match *pattern {
                // Apparently inline blocks also don't include the prototype when within the prototype.
                // This is really weird, but necessary to run the YAML syntax.
                Pattern::Match(ref match_pat) => {
                    let maybe_context_refs = match match_pat.operation {
                        MatchOperation::Push(ref context_refs) |
                        MatchOperation::Set(ref context_refs) => Some(context_refs),
                        MatchOperation::Pop | MatchOperation::None => None,
                    };
                    if let Some(context_refs) = maybe_context_refs {
                        for context_ref in context_refs.iter() {
                            match context_ref {
                                ContextReference::Inline(ref s) | ContextReference::Named(ref s) => {
                                    if let Some(i) = syntax.contexts.get(s) {
                                        Self::recursively_mark_no_prototype(syntax, i.index(), contexts, no_prototype);
                                    }
                                },
                                _ => (),
                            }
                        }
                    }
                }
                Pattern::Include(ContextReference::Named(ref s)) => {
                    if let Some(i) = syntax.contexts.get(s) {
                        Self::recursively_mark_no_prototype(syntax, i.index(), contexts, no_prototype);
                    }
                }
                _ => (),
            }
        }
    }

    fn link_context(context: &mut Context, syntax: &SyntaxReference, syntaxes: &[SyntaxReference]) {
        for pattern in &mut context.patterns {
            match *pattern {
                Pattern::Match(ref mut match_pat) => Self::link_match_pat(match_pat, syntax, syntaxes),
                Pattern::Include(ref mut context_ref) => Self::link_ref(context_ref, syntax, syntaxes),
            }
        }
    }

    fn link_ref(context_ref: &mut ContextReference, syntax: &SyntaxReference, syntaxes: &[SyntaxReference]) {
        // println!("{:?}", context_ref);
        use super::syntax_definition::ContextReference::*;
        let linked_context_id = match *context_ref {
            Named(ref s) | Inline(ref s) => {
                // This isn't actually correct, but it is better than nothing/crashing.
                // This is being phased out anyhow, see https://github.com/sublimehq/Packages/issues/73
                // Fixes issue #30
                if s == "$top_level_main" {
                    syntax.contexts.get("main")
                } else {
                    syntax.contexts.get(s)
                }
            }
            ByScope { scope, ref sub_context } => {
                let context_name = sub_context.as_ref().map_or("main", |x| &**x);
                syntaxes
                    .iter()
                    .find(|s| s.scope == scope)
                    .and_then(|s| s.contexts.get(context_name))
            }
            File { ref name, ref sub_context } => {
                let context_name = sub_context.as_ref().map_or("main", |x| &**x);
                syntaxes
                    .iter()
                    .find(|s| &s.name == name)
                    .and_then(|s| s.contexts.get(context_name))
            }
            Direct(_) => None,
        };
        if let Some(context_id) = linked_context_id {
            let mut new_ref = Direct(context_id.clone());
            mem::swap(context_ref, &mut new_ref);
        }
    }

    fn link_match_pat(match_pat: &mut MatchPattern, syntax: &SyntaxReference, syntaxes: &[SyntaxReference]) {
        let maybe_context_refs = match match_pat.operation {
            MatchOperation::Push(ref mut context_refs) |
            MatchOperation::Set(ref mut context_refs) => Some(context_refs),
            MatchOperation::Pop | MatchOperation::None => None,
        };
        if let Some(context_refs) = maybe_context_refs {
            for context_ref in context_refs.iter_mut() {
                Self::link_ref(context_ref, syntax, syntaxes);
            }
        }
        if let Some(ref mut context_ref) = match_pat.with_prototype {
            Self::link_ref(context_ref, syntax, syntaxes);
        }
    }
}

#[derive(Debug)]
struct FirstLineCache {
    /// (first line regex, syntax index) pairs for all syntaxes with a first line regex
    /// built lazily on first use of `find_syntax_by_first_line`.
    regexes: Vec<(Regex, usize)>,
    /// To what extent the first line cache has been built
    cached_until: usize,
}

impl Default for FirstLineCache {
    fn default() -> Self {
        FirstLineCache {
            regexes: Vec::new(),
            cached_until: 0,
        }
    }
}

impl FirstLineCache {
    fn new() -> FirstLineCache {
        FirstLineCache::default()
    }

    fn ensure_filled(&mut self, syntaxes: &[SyntaxReference]) {
        if self.cached_until >= syntaxes.len() {
            return;
        }

        for (i, syntax) in syntaxes[self.cached_until..].iter().enumerate() {
            if let Some(ref reg_str) = syntax.first_line_match {
                if let Ok(reg) = Regex::new(reg_str) {
                    self.regexes.push((reg, i));
                }
            }
        }

        self.cached_until = syntaxes.len();
    }
}


#[cfg(feature = "yaml-load")]
#[cfg(test)]
mod tests {
    use super::*;
    use parsing::{ParseState, Scope, syntax_definition};
    use std::collections::HashMap;

    #[test]
    fn can_load() {
        let mut builder = SyntaxSetBuilder::new();
        builder.load_syntaxes("testdata/Packages", false).unwrap();

        let cmake_dummy_syntax = SyntaxDefinition {
            name: "CMake".to_string(),
            file_extensions: vec!["CMakeLists.txt".to_string(), "cmake".to_string()],
            scope: Scope::new("source.cmake").unwrap(),
            first_line_match: None,
            hidden: false,
            variables: HashMap::new(),
            contexts: HashMap::new(),
        };

        builder.add_syntax(cmake_dummy_syntax);
        builder.load_plain_text_syntax();

        let ps = builder.build();

        assert_eq!(&ps.find_syntax_by_first_line("#!/usr/bin/env node").unwrap().name,
                   "JavaScript");
        let rails_scope = Scope::new("source.ruby.rails").unwrap();
        let syntax = ps.find_syntax_by_name("Ruby on Rails").unwrap();
        ps.find_syntax_plain_text();
        assert_eq!(&ps.find_syntax_by_extension("rake").unwrap().name, "Ruby");
        assert_eq!(&ps.find_syntax_by_token("ruby").unwrap().name, "Ruby");
        assert_eq!(&ps.find_syntax_by_first_line("lol -*- Mode: C -*- such line").unwrap().name,
                   "C");
        assert_eq!(&ps.find_syntax_for_file("testdata/parser.rs").unwrap().unwrap().name,
                   "Rust");
        assert_eq!(&ps.find_syntax_for_file("testdata/test_first_line.test")
                       .unwrap()
                       .unwrap()
                       .name,
                   "Go");
        assert_eq!(&ps.find_syntax_for_file(".bashrc").unwrap().unwrap().name,
                   "Bourne Again Shell (bash)");
        assert_eq!(&ps.find_syntax_for_file("CMakeLists.txt").unwrap().unwrap().name,
                   "CMake");
        assert_eq!(&ps.find_syntax_for_file("test.cmake").unwrap().unwrap().name,
                   "CMake");
        assert_eq!(&ps.find_syntax_for_file("Rakefile").unwrap().unwrap().name, "Ruby");
        assert!(&ps.find_syntax_by_first_line("derp derp hi lol").is_none());
        assert_eq!(&ps.find_syntax_by_path("Packages/Rust/Rust.sublime-syntax").unwrap().name,
                   "Rust");
        // println!("{:#?}", syntax);
        assert_eq!(syntax.scope, rails_scope);
        // assert!(false);
        let main_context = ps.get_context(&syntax.contexts["main"]);
        let count = syntax_definition::context_iter(&ps, main_context).count();
        assert_eq!(count, 109);
    }

    #[test]
    fn can_clone() {
        let cloned_syntax_set = {
            let mut builder = SyntaxSetBuilder::new();
            builder.add_syntax(syntax_a());
            builder.add_syntax(syntax_b());

            let syntax_set_original = builder.build();
            syntax_set_original.clone()
            // Note: The original syntax set is dropped
        };

        let syntax = cloned_syntax_set.find_syntax_by_extension("a").unwrap();
        let mut parse_state = ParseState::new(&cloned_syntax_set, syntax);
        let ops = parse_state.parse_line("a go_b b");
        let expected = (7, ScopeStackOp::Push(Scope::new("b").unwrap()));
        assert_ops_contain(&ops, &expected);
    }

    #[test]
    fn can_add_more_syntaxes_with_builder() {
        let syntax_set_original = {
            let mut builder = SyntaxSetBuilder::new();
            builder.add_syntax(syntax_a());
            builder.add_syntax(syntax_b());
            builder.build()
        };

        let mut builder = syntax_set_original.into_builder();

        let syntax_c = SyntaxDefinition::load_from_str(r#"
        name: C
        scope: source.c
        file_extensions: [c]
        contexts:
          main:
            - match: 'c'
              scope: c
            - match: 'go_a'
              push: scope:source.a#main
        "#, true, None).unwrap();

        builder.add_syntax(syntax_c);

        let syntax_set = builder.build();

        let syntax = syntax_set.find_syntax_by_extension("c").unwrap();
        let mut parse_state = ParseState::new(&syntax_set, syntax);
        let ops = parse_state.parse_line("c go_a a go_b b");
        let expected = (14, ScopeStackOp::Push(Scope::new("b").unwrap()));
        assert_ops_contain(&ops, &expected);
    }

    #[test]
    fn can_use_in_multiple_threads() {
        use rayon::prelude::*;

        let syntax_set = {
            let mut builder = SyntaxSetBuilder::new();
            builder.add_syntax(syntax_a());
            builder.add_syntax(syntax_b());
            builder.build()
        };

        let lines = vec![
            "a a a",
            "a go_b b",
            "go_b b",
            "go_b b  b",
        ];

        let results: Vec<Vec<(usize, ScopeStackOp)>> = lines
            .par_iter()
            .map(|line| {
                let syntax = syntax_set.find_syntax_by_extension("a").unwrap();
                let mut parse_state = ParseState::new(&syntax_set, syntax);
                parse_state.parse_line(line)
            })
            .collect();

        assert_ops_contain(&results[0], &(4, ScopeStackOp::Push(Scope::new("a").unwrap())));
        assert_ops_contain(&results[1], &(7, ScopeStackOp::Push(Scope::new("b").unwrap())));
        assert_ops_contain(&results[2], &(5, ScopeStackOp::Push(Scope::new("b").unwrap())));
        assert_ops_contain(&results[3], &(8, ScopeStackOp::Push(Scope::new("b").unwrap())));
    }

    #[test]
    fn is_sync() {
        check_sync::<SyntaxSet>();
    }

    #[test]
    fn is_send() {
        check_send::<SyntaxSet>();
    }

    fn assert_ops_contain(
        ops: &[(usize, ScopeStackOp)],
        expected: &(usize, ScopeStackOp)
    ) {
        assert!(ops.contains(expected),
                "expected operations to contain {:?}: {:?}", expected, ops);
    }

    fn check_send<T: Send>() {}

    fn check_sync<T: Sync>() {}

    fn syntax_a() -> SyntaxDefinition {
        SyntaxDefinition::load_from_str(
            r#"
            name: A
            scope: source.a
            file_extensions: [a]
            contexts:
              main:
                - match: 'a'
                  scope: a
                - match: 'go_b'
                  push: scope:source.b#main
            "#,
            true,
            None,
        ).unwrap()
    }

    fn syntax_b() -> SyntaxDefinition {
        SyntaxDefinition::load_from_str(
            r#"
            name: B
            scope: source.b
            file_extensions: [b]
            contexts:
              main:
                - match: 'b'
                  scope: b
            "#,
            true,
            None,
        ).unwrap()
    }
}
