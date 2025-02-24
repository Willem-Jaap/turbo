use anyhow::{anyhow, bail, Result};
use lazy_static::lazy_static;
use swc_core::{
    common::DUMMY_SP,
    ecma::ast::{self, Expr, ExprStmt, Ident, Lit, ModuleItem, Program, Script, Stmt},
    quote,
};
use turbo_tasks::{Value, ValueToString, Vc};
use turbopack_core::{
    chunk::{
        ChunkItemExt, ChunkableModule, ChunkableModuleReference, ChunkingContext, ChunkingType,
        ChunkingTypeOption, ModuleId,
    },
    issue::{IssueSeverity, IssueSource},
    module::Module,
    reference::ModuleReference,
    reference_type::EcmaScriptModulesReferenceSubType,
    resolve::{
        origin::{ResolveOrigin, ResolveOriginExt},
        parse::Request,
        ModulePart, ModuleResolveResult, ModuleResolveResultItem,
    },
};

use crate::{
    analyzer::imports::ImportAnnotations,
    chunk::{EcmascriptChunkPlaceable, EcmascriptChunkingContext},
    code_gen::{CodeGenerateable, CodeGeneration},
    create_visitor, magic_identifier,
    references::util::{request_to_string, throw_module_not_found_expr},
    resolve::esm_resolve,
};

#[turbo_tasks::value]
pub enum ReferencedAsset {
    Some(Vc<Box<dyn EcmascriptChunkPlaceable>>),
    OriginalReferenceTypeExternal(String),
    None,
}

impl ReferencedAsset {
    pub async fn get_ident(&self) -> Result<Option<String>> {
        Ok(match self {
            ReferencedAsset::Some(asset) => Some(Self::get_ident_from_placeable(asset).await?),
            ReferencedAsset::OriginalReferenceTypeExternal(request) => {
                Some(magic_identifier::mangle(&format!("external {}", request)))
            }
            ReferencedAsset::None => None,
        })
    }

    pub(crate) async fn get_ident_from_placeable(
        asset: &Vc<Box<dyn EcmascriptChunkPlaceable>>,
    ) -> Result<String> {
        let path = asset.ident().to_string().await?;
        Ok(magic_identifier::mangle(&format!(
            "imported module {}",
            path
        )))
    }
}

#[turbo_tasks::value_impl]
impl ReferencedAsset {
    #[turbo_tasks::function]
    pub async fn from_resolve_result(resolve_result: Vc<ModuleResolveResult>) -> Result<Vc<Self>> {
        // TODO handle multiple keyed results
        for (_key, result) in resolve_result.await?.primary.iter() {
            match result {
                ModuleResolveResultItem::OriginalReferenceTypeExternal(request) => {
                    return Ok(
                        ReferencedAsset::OriginalReferenceTypeExternal(request.clone()).cell(),
                    );
                }
                &ModuleResolveResultItem::Module(module) => {
                    if let Some(placeable) =
                        Vc::try_resolve_downcast::<Box<dyn EcmascriptChunkPlaceable>>(module)
                            .await?
                    {
                        return Ok(ReferencedAsset::cell(ReferencedAsset::Some(placeable)));
                    }
                }
                // TODO ignore should probably be handled differently
                _ => {}
            }
        }
        Ok(ReferencedAsset::cell(ReferencedAsset::None))
    }
}

#[turbo_tasks::value]
#[derive(Hash, Debug)]
pub struct EsmAssetReference {
    pub origin: Vc<Box<dyn ResolveOrigin>>,
    pub request: Vc<Request>,
    pub annotations: ImportAnnotations,
    pub issue_source: Option<Vc<IssueSource>>,
    pub export_name: Option<Vc<ModulePart>>,
    pub import_externals: bool,
}

/// A list of [EsmAssetReference]s
#[turbo_tasks::value(transparent)]
pub struct EsmAssetReferences(Vec<Vc<EsmAssetReference>>);

impl EsmAssetReference {
    fn get_origin(&self) -> Vc<Box<dyn ResolveOrigin>> {
        let mut origin = self.origin;
        if let Some(transition) = self.annotations.transition() {
            origin = origin.with_transition(transition.to_string());
        }
        origin
    }
}

#[turbo_tasks::value_impl]
impl EsmAssetReference {
    #[turbo_tasks::function]
    pub fn new(
        origin: Vc<Box<dyn ResolveOrigin>>,
        request: Vc<Request>,
        issue_source: Option<Vc<IssueSource>>,
        annotations: Value<ImportAnnotations>,
        export_name: Option<Vc<ModulePart>>,
        import_externals: bool,
    ) -> Vc<Self> {
        Self::cell(EsmAssetReference {
            origin,
            request,
            issue_source,
            annotations: annotations.into_value(),
            export_name,
            import_externals,
        })
    }

    #[turbo_tasks::function]
    pub(crate) fn get_referenced_asset(self: Vc<Self>) -> Vc<ReferencedAsset> {
        ReferencedAsset::from_resolve_result(self.resolve_reference())
    }
}

#[turbo_tasks::value_impl]
impl ModuleReference for EsmAssetReference {
    #[turbo_tasks::function]
    async fn resolve_reference(&self) -> Result<Vc<ModuleResolveResult>> {
        let ty = Value::new(match &self.export_name {
            Some(part) => EcmaScriptModulesReferenceSubType::ImportPart(*part),
            None => EcmaScriptModulesReferenceSubType::Import,
        });

        Ok(esm_resolve(
            self.get_origin().resolve().await?,
            self.request,
            ty,
            IssueSeverity::Error.cell(),
            self.issue_source,
        ))
    }
}

#[turbo_tasks::value_impl]
impl ValueToString for EsmAssetReference {
    #[turbo_tasks::function]
    async fn to_string(&self) -> Result<Vc<String>> {
        Ok(Vc::cell(format!(
            "import {} {}",
            self.request.to_string().await?,
            self.annotations
        )))
    }
}

#[turbo_tasks::value_impl]
impl ChunkableModuleReference for EsmAssetReference {
    #[turbo_tasks::function]
    fn chunking_type(&self) -> Result<Vc<ChunkingTypeOption>> {
        Ok(Vc::cell(
            if let Some(chunking_type) = self.annotations.chunking_type() {
                match chunking_type {
                    "parallel" => Some(ChunkingType::ParallelInheritAsync),
                    "none" => None,
                    _ => return Err(anyhow!("unknown chunking_type: {}", chunking_type)),
                }
            } else {
                Some(ChunkingType::ParallelInheritAsync)
            },
        ))
    }
}

#[turbo_tasks::value_impl]
impl CodeGenerateable for EsmAssetReference {
    #[turbo_tasks::function]
    async fn code_generation(
        self: Vc<Self>,
        chunking_context: Vc<Box<dyn EcmascriptChunkingContext>>,
    ) -> Result<Vc<CodeGeneration>> {
        let mut visitors = Vec::new();

        let this = &*self.await?;
        let chunking_type = self.chunking_type().await?;
        let resolved = self.resolve_reference().await?;

        // Insert code that throws immediately at time of import if a request is
        // unresolvable
        if resolved.is_unresolveable_ref() {
            let request = request_to_string(this.request).await?.to_string();
            visitors.push(create_visitor!(visit_mut_program(program: &mut Program) {
                insert_hoisted_stmt(program, Stmt::Expr(ExprStmt {
                        expr: Box::new(throw_module_not_found_expr(
                          &request
                        )),
                        span: DUMMY_SP,
                    }));
            }));

            return Ok(CodeGeneration { visitors }.into());
        }

        // only chunked references can be imported
        if chunking_type.is_some() {
            let referenced_asset = self.get_referenced_asset().await?;
            let import_externals = this.import_externals;
            if let Some(ident) = referenced_asset.get_ident().await? {
                match &*referenced_asset {
                    ReferencedAsset::Some(asset) => {
                        let id = asset
                            .as_chunk_item(Vc::upcast(chunking_context))
                            .id()
                            .await?;
                        visitors.push(create_visitor!(visit_mut_program(program: &mut Program) {
                            let stmt = quote!(
                                "var $name = __turbopack_import__($id);" as Stmt,
                                name = Ident::new(ident.clone().into(), DUMMY_SP),
                                id: Expr = Expr::Lit(match &*id {
                                    ModuleId::String(s) => s.clone().into(),
                                    ModuleId::Number(n) => (*n as f64).into(),
                                })
                            );
                            insert_hoisted_stmt(program, stmt);
                        }));
                    }
                    ReferencedAsset::OriginalReferenceTypeExternal(request) => {
                        if !*chunking_context
                            .environment()
                            .supports_commonjs_externals()
                            .await?
                        {
                            bail!(
                                "the chunking context does not support external modules (request: \
                                 {})",
                                request
                            );
                        }
                        let request = request.clone();
                        visitors.push(create_visitor!(visit_mut_program(program: &mut Program) {
                            // TODO Technically this should insert a ESM external, but we don't support that yet
                            let stmt = if import_externals {
                                quote!(
                                    "var $name = __turbopack_external_import__($id);" as Stmt,
                                    name = Ident::new(ident.clone().into(), DUMMY_SP),
                                    id: Expr = Expr::Lit(request.clone().into())
                                )
                            } else {
                                quote!(
                                    "var $name = __turbopack_external_require__($id, true);" as Stmt,
                                    name = Ident::new(ident.clone().into(), DUMMY_SP),
                                    id: Expr = Expr::Lit(request.clone().into())
                                )
                            };
                            insert_hoisted_stmt(program, stmt);
                        }));
                    }
                    ReferencedAsset::None => {}
                }
            }
        }

        Ok(CodeGeneration { visitors }.into())
    }
}

lazy_static! {
    static ref ESM_HOISTING_LOCATION: &'static str = Box::leak(Box::new(magic_identifier::mangle(
        "ecmascript hoisting location"
    )));
}

pub(crate) fn insert_hoisted_stmt(program: &mut Program, stmt: Stmt) {
    match program {
        Program::Module(ast::Module { body, .. }) => {
            let pos = body.iter().position(|item| {
                if let ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                    expr: box Expr::Lit(Lit::Str(s)),
                    ..
                })) = item
                {
                    &*s.value == *ESM_HOISTING_LOCATION
                } else {
                    false
                }
            });
            if let Some(pos) = pos {
                let has_stmt = body[0..pos].iter().any(|item| {
                    if let ModuleItem::Stmt(item_stmt) = item {
                        stmt == *item_stmt
                    } else {
                        false
                    }
                });
                if !has_stmt {
                    body.insert(pos, ModuleItem::Stmt(stmt));
                }
            } else {
                body.splice(
                    0..0,
                    [
                        ModuleItem::Stmt(stmt),
                        ModuleItem::Stmt(Stmt::Expr(ExprStmt {
                            expr: Box::new(Expr::Lit(Lit::Str((*ESM_HOISTING_LOCATION).into()))),
                            span: DUMMY_SP,
                        })),
                    ],
                );
            }
        }
        Program::Script(Script { body, .. }) => {
            let pos = body.iter().position(|item| {
                if let Stmt::Expr(ExprStmt {
                    expr: box Expr::Lit(Lit::Str(s)),
                    ..
                }) = item
                {
                    &*s.value == *ESM_HOISTING_LOCATION
                } else {
                    false
                }
            });
            if let Some(pos) = pos {
                body.insert(pos, stmt);
            } else {
                body.insert(
                    0,
                    Stmt::Expr(ExprStmt {
                        expr: Box::new(Expr::Lit(Lit::Str((*ESM_HOISTING_LOCATION).into()))),
                        span: DUMMY_SP,
                    }),
                );
                body.insert(0, stmt);
            }
        }
    }
}
