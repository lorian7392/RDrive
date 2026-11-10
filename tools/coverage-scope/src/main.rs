//! Find test-only source spans using Rust syntax, without excluding production
//! errors, adapters, platform code or whole partially-covered source files.
use proc_macro2::Span;
use serde_json::{json, Value};
use std::{collections::BTreeMap, error::Error, fs, path::{Path, PathBuf}};
use syn::{punctuated::Punctuated, spanned::Spanned, visit::Visit, Attribute, Item, Meta, Token};
type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn requires_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) => {
            let Ok(parts) = list.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated) else { return false; };
            if list.path.is_ident("all") { parts.iter().any(requires_test) }
            else if list.path.is_ident("any") { !parts.is_empty() && parts.iter().all(requires_test) }
            else { false }
        }
        _ => false,
    }
}
fn test_only(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if attr.path().segments.last().is_some_and(|segment| segment.ident == "test") { return true; }
        attr.path().is_ident("cfg") && attr.parse_args::<Meta>().is_ok_and(|meta| requires_test(&meta))
    })
}
fn attributes(item: &Item) -> &[Attribute] {
    match item {
        Item::Const(i) => &i.attrs, Item::Enum(i) => &i.attrs, Item::ExternCrate(i) => &i.attrs,
        Item::Fn(i) => &i.attrs, Item::ForeignMod(i) => &i.attrs, Item::Impl(i) => &i.attrs,
        Item::Macro(i) => &i.attrs, Item::Mod(i) => &i.attrs, Item::Static(i) => &i.attrs,
        Item::Struct(i) => &i.attrs, Item::Trait(i) => &i.attrs, Item::TraitAlias(i) => &i.attrs,
        Item::Type(i) => &i.attrs, Item::Union(i) => &i.attrs, Item::Use(i) => &i.attrs,
        _ => &[],
    }
}
struct Scope { file: PathBuf, ranges: Vec<Value>, external: Vec<PathBuf>, production_functions: usize }
impl Scope {
    fn range(&mut self, span: Span) { self.ranges.push(json!({"start":span.start().line,"end":span.end().line,"reason":"Rust item guaranteed to require cfg(test) or a test attribute"})); }
}
impl<'ast> Visit<'ast> for Scope {
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        self.production_functions += 1;
        syn::visit::visit_item_fn(self, item);
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        if test_only(&item.attrs) { self.range(item.span()); return; }
        self.production_functions += 1;
        syn::visit::visit_impl_item_fn(self, item);
    }
    fn visit_trait_item_fn(&mut self, item: &'ast syn::TraitItemFn) {
        if test_only(&item.attrs) { self.range(item.span()); return; }
        if item.default.is_some() { self.production_functions += 1; }
        syn::visit::visit_trait_item_fn(self, item);
    }
    fn visit_item(&mut self, item: &'ast Item) {
        if test_only(attributes(item)) {
            self.range(item.span());
            if let Item::Mod(module) = item {
                if module.content.is_none() {
                    let explicit = module.attrs.iter().find_map(|attribute| {
                        if !attribute.path().is_ident("path") { return None; }
                        let Meta::NameValue(value) = &attribute.meta else { return None; };
                        let syn::Expr::Lit(value) = &value.value else { return None; };
                        let syn::Lit::Str(value) = &value.lit else { return None; };
                        Some(self.file.parent().unwrap().join(value.value()))
                    });
                    if let Some(path) = explicit { self.external.push(path); }
                    else {
                        let parent=self.file.parent().unwrap();
                        let stem=self.file.file_stem().unwrap().to_string_lossy();
                        let base=if ["lib","main","mod"].contains(&stem.as_ref()){parent.to_path_buf()}else{parent.join(stem.as_ref())};
                        let leaf=base.join(format!("{}.rs",module.ident));
                        self.external.push(if leaf.exists(){leaf}else{base.join(module.ident.to_string()).join("mod.rs")});
                    }
                }
            }
        } else { syn::visit::visit_item(self,item); }
    }
}
fn files(directory: &Path, output: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let path=entry?.path();
        if path.is_dir() { files(&path,output)?; }
        else if path.extension().is_some_and(|extension|extension=="rs") { output.push(path); }
    }
    Ok(())
}
fn main() -> Result<()> {
    let root=std::env::args_os().nth(1).map(PathBuf::from).unwrap_or(std::env::current_dir()?).canonicalize()?;
    let mut source=Vec::new();files(&root.join("src"),&mut source)?;source.sort();
    let mut ranges=BTreeMap::<String,Vec<Value>>::new();let mut functions=BTreeMap::<String,usize>::new();let mut external=Vec::new();
    for file in source {
        let text=fs::read_to_string(&file)?;let syntax=syn::parse_file(&text)?;
        let mut scope=Scope{file:file.clone(),ranges:Vec::new(),external:Vec::new(),production_functions:0};scope.visit_file(&syntax);
        let name=file.strip_prefix(&root)?.to_string_lossy().into_owned();
        functions.insert(name.clone(),scope.production_functions);
        ranges.insert(name,scope.ranges);external.extend(scope.external);
    }
    for file in external {
        let text=fs::read_to_string(&file)?;
        ranges.insert(file.strip_prefix(&root)?.to_string_lossy().into_owned(),vec![json!({"start":1,"end":text.lines().count(),"reason":"external module declared with cfg(test)"})]);
    }
    println!("{}",serde_json::to_string_pretty(&json!({"method":"syn AST: exclude only test-required items, with no production omission","files":ranges,"production_function_items":functions}))?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test] fn conditional_test_requirement_is_conservative() {
        for expression in ["test","all(test,target_os=\"macos\")","any(test,all(test,feature=\"x\"))"] { assert!(requires_test(&syn::parse_str(expression).unwrap())); }
        for expression in ["not(test)","any(test,target_os=\"macos\")","all(not(test),target_os=\"macos\")","target_os=\"macos\""] { assert!(!requires_test(&syn::parse_str(expression).unwrap())); }
    }
    #[test] fn strings_comments_and_production_errors_are_never_test_exclusions() {
        let code=r##"fn production() { let _ = "#[cfg(test)] {"; /* } */ panic!("failure"); }
#[cfg(test)] mod tests { #[test] fn case() { let _ = r#"}"#; } }
#[tokio::test] async fn another_case() {}
#[cfg(not(test))] fn normal_binary() {}"##;
        let parsed=syn::parse_file(code).unwrap();let mut scope=Scope{file:PathBuf::from("lib.rs"),ranges:vec![],external:vec![],production_functions:0};scope.visit_file(&parsed);
        assert_eq!(scope.ranges.len(),2);assert_eq!(scope.ranges[0]["start"],2);assert_eq!(scope.ranges[1]["start"],3);
    }
}
