use crate::ast_visit::{DeclarationSiteVisitor, visit_declaration_sites};
use crate::parser::{Parameter, Stmt, VariantDecl, VariantKind};

#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::collections::{BTreeMap, BTreeSet};
#[cfg(all(feature = "no_std", not(feature = "std")))]
use alloc::{
    string::{String, ToString},
    vec,
    vec::Vec,
};
#[cfg(any(feature = "std", not(feature = "no_std")))]
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct RuntimeTypeId {
    pub(super) module_path: String,
    pub(super) type_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct DeclSiteId {
    pub(super) module_path: String,
    ordinal: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EnumShape {
    pub(super) variants: Vec<EnumVariantShape>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct EnumVariantShape {
    pub(super) name: String,
    payload: EnumPayloadShape,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum EnumPayloadShape {
    Unit,
    Tuple(usize),
    Struct(Vec<String>),
}

impl EnumShape {
    pub(super) fn from_variants(variants: &[VariantDecl]) -> Self {
        Self {
            variants: variants
                .iter()
                .map(|variant| EnumVariantShape {
                    name: variant.name.clone(),
                    payload: match &variant.kind {
                        VariantKind::Unit => EnumPayloadShape::Unit,
                        VariantKind::Tuple(fields) => EnumPayloadShape::Tuple(fields.len()),
                        VariantKind::Struct(fields) => EnumPayloadShape::Struct(fields.clone()),
                    },
                })
                .collect(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct KnownEnum {
    pub(super) runtime_id: RuntimeTypeId,
    pub(super) site: DeclSiteId,
    pub(super) shape: EnumShape,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum TypeBinding {
    Known(KnownEnum),
    Ambiguous,
    Opaque,
}

pub(super) fn merge_possible_types(left: &TypeBinding, right: &TypeBinding) -> TypeBinding {
    match (left, right) {
        (TypeBinding::Known(a), TypeBinding::Known(b))
            if a.runtime_id == b.runtime_id && a.shape == b.shape =>
        {
            TypeBinding::Known(a.clone())
        }
        (TypeBinding::Opaque, _) | (_, TypeBinding::Opaque) => TypeBinding::Opaque,
        _ => TypeBinding::Ambiguous,
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct ModuleSurface {
    pub(super) types: BTreeMap<String, TypeBinding>,
    pub(super) namespaces: BTreeMap<String, NamespaceBinding>,
    pub(super) value_names: BTreeSet<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum NamespaceBinding {
    Known(ModuleSurface),
    Opaque,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ModuleKnowledge {
    Known(ModuleSurface),
    Opaque,
}

#[derive(Clone, Debug, Default)]
pub(super) struct Frame {
    pub(super) types: BTreeMap<String, TypeBinding>,
    pub(super) namespaces: BTreeMap<String, NamespaceBinding>,
    pub(super) value_shadows: BTreeSet<String>,
    pub(super) alias_blockers: BTreeSet<String>,
}

impl Frame {
    pub(super) fn shadow_namespace_with_value(&mut self, name: &str) {
        self.value_shadows.insert(name.to_string());
        self.namespaces
            .insert(name.to_string(), NamespaceBinding::Opaque);
    }
}

#[derive(Clone, Debug)]
pub(super) struct LexicalEnv {
    pub(super) frames: Vec<Frame>,
}

impl LexicalEnv {
    pub(super) fn direct() -> Self {
        Self {
            frames: vec![Frame::default()],
        }
    }

    pub(super) fn callable(base: &Frame, params: &[String]) -> Self {
        let mut env = Self {
            frames: vec![base.clone(), Frame::default()],
        };
        for param in params {
            env.shadow_namespace_with_value(param);
        }
        env
    }

    pub(super) fn push_frame(&mut self) {
        self.frames.push(Frame::default());
    }

    fn current_mut(&mut self) -> &mut Frame {
        self.frames.last_mut().expect("lexical environment frame")
    }

    pub(super) fn bind_type(&mut self, name: &str, binding: TypeBinding) {
        self.current_mut().types.insert(name.to_string(), binding);
    }

    pub(super) fn bind_imported_type(&mut self, name: &str, binding: TypeBinding) {
        self.current_mut()
            .types
            .entry(name.to_string())
            .or_insert(binding);
    }

    pub(super) fn bind_imported_namespace(&mut self, name: &str, binding: NamespaceBinding) {
        let frame = self.current_mut();
        if frame.value_shadows.contains(name) || frame.alias_blockers.contains(name) {
            frame
                .namespaces
                .insert(name.to_string(), NamespaceBinding::Opaque);
        } else {
            frame.namespaces.entry(name.to_string()).or_insert(binding);
        }
    }

    pub(super) fn bind_imported_value_name(&mut self, name: &str) {
        let frame = self.current_mut();
        if !frame.namespaces.contains_key(name) {
            frame.alias_blockers.insert(name.to_string());
        }
    }

    pub(super) fn shadow_namespace_with_value(&mut self, name: &str) {
        self.current_mut().shadow_namespace_with_value(name);
    }

    pub(super) fn block_future_namespace(&mut self, name: &str) {
        self.current_mut().alias_blockers.insert(name.to_string());
    }

    pub(super) fn resolve_type(&self, name: &str) -> Option<&TypeBinding> {
        self.frames
            .iter()
            .rev()
            .find_map(|frame| frame.types.get(name))
    }

    pub(super) fn resolve_namespace_type(
        &self,
        namespace: &str,
        name: &str,
    ) -> Option<&TypeBinding> {
        for frame in self.frames.iter().rev() {
            if frame.value_shadows.contains(namespace) {
                return None;
            }
            if let Some(binding) = frame.namespaces.get(namespace) {
                return match binding {
                    NamespaceBinding::Known(surface) => surface.types.get(name),
                    NamespaceBinding::Opaque => None,
                };
            }
        }
        None
    }

    pub(super) fn invalidate_visible_namespace(&mut self, name: &str) -> Option<usize> {
        for (index, frame) in self.frames.iter_mut().enumerate().rev() {
            if frame.value_shadows.contains(name) {
                return None;
            }
            if frame.namespaces.contains_key(name) {
                frame
                    .namespaces
                    .insert(name.to_string(), NamespaceBinding::Opaque);
                return Some(index);
            }
        }
        None
    }
}

#[derive(Clone, Debug, Default)]
pub(super) struct FlowEffects {
    pub(super) invalidated_namespaces: BTreeMap<String, usize>,
    pub(super) alias_blockers: BTreeSet<String>,
}

impl FlowEffects {
    pub(super) fn merge(&mut self, other: Self) {
        for (name, frame) in other.invalidated_namespaces {
            self.invalidated_namespaces
                .entry(name)
                .and_modify(|current| *current = (*current).min(frame))
                .or_insert(frame);
        }
        self.alias_blockers.extend(other.alias_blockers);
    }
}

pub(super) fn apply_inherited_effects(env: &mut LexicalEnv, effects: &mut FlowEffects) {
    let parent_depth = env.frames.len();
    effects.invalidated_namespaces.retain(|name, frame| {
        if *frame < parent_depth {
            env.invalidate_visible_namespace(name);
            true
        } else {
            false
        }
    });
    for name in &effects.alias_blockers {
        env.block_future_namespace(name);
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct SiteKey {
    line: u32,
    column: u32,
    name: String,
}

#[derive(Clone, Debug, Default)]
pub(super) struct SiteCatalogue {
    enums: BTreeMap<SiteKey, DeclSiteId>,
}

struct SiteCollector<'a> {
    module_path: &'a str,
    next_ordinal: usize,
    catalogue: SiteCatalogue,
}

impl SiteCatalogue {
    pub(super) fn discover(stmts: &[Stmt], module_path: &str) -> Self {
        let mut collector = SiteCollector {
            module_path,
            next_ordinal: 0,
            catalogue: Self::default(),
        };
        visit_declaration_sites(stmts, &mut collector);
        collector.catalogue
    }

    pub(super) fn enum_site(&self, name: &str, stmt: &Stmt) -> DeclSiteId {
        self.enums
            .get(&SiteKey {
                line: stmt.line,
                column: stmt.column.map(core::num::NonZeroU32::get).unwrap_or(0),
                name: name.to_string(),
            })
            .cloned()
            .expect("enum declaration must have a discovered site")
    }
}

impl DeclarationSiteVisitor for SiteCollector<'_> {
    fn visit_struct(&mut self, _name: &str, _fields: &[String], _stmt: &Stmt) {
        self.next_ordinal += 1;
    }

    fn visit_enum(&mut self, name: &str, _variants: &[VariantDecl], stmt: &Stmt) {
        let site = DeclSiteId {
            module_path: self.module_path.to_string(),
            ordinal: self.next_ordinal,
        };
        self.next_ordinal += 1;
        self.catalogue.enums.insert(
            SiteKey {
                line: stmt.line,
                column: stmt.column.map(core::num::NonZeroU32::get).unwrap_or(0),
                name: name.to_string(),
            },
            site,
        );
    }

    fn visit_method(
        &mut self,
        _type_name: &str,
        _method_name: &str,
        _params: &[Parameter],
        _body: &[Stmt],
        _stmt: &Stmt,
    ) {
        self.next_ordinal += 1;
    }
}
