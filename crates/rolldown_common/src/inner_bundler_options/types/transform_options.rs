use std::{
  ops::{Deref, DerefMut},
  path::{Path, PathBuf},
  sync::Arc,
};

use dashmap::DashMap;
use itertools::Either;
use oxc::transformer::{EngineTargets, TransformOptions as OxcTransformOptions};
use rolldown_error::{BuildDiagnostic, BuildResult};

use crate::{BundlerTransformOptions, JsxOptions};

#[derive(Debug, Default, Clone)]
pub enum JsxPreset {
  // Enable jsx transformer.
  #[default]
  Enable,
  // Disable jsx parser, it will give you a syntax error if you use jsx syntax
  Disable,
  // Disable jsx transformer.
  Preserve,
}

#[derive(Debug, Clone)]
pub enum TransformOptionsInner {
  /// Raw mode: Store BundlerTransformOptions and cache resolved OxcTransformOptions per tsconfig path.
  /// Used when TsConfig::Auto is set, so each file can use its nearest tsconfig.
  Raw((Arc<BundlerTransformOptions>, Arc<DashMap<PathBuf, Arc<OxcTransformOptions>>>)),
  /// Normal mode: Pre-resolved OxcTransformOptions for all files.
  /// Used when TsConfig is None or Special(path).
  Normal(Arc<OxcTransformOptions>),
}

#[derive(Debug, Clone)]
pub struct TransformOptions {
  inner: TransformOptionsInner,
  pub target: EngineTargets,
  pub jsx_preset: JsxPreset,
}

impl Deref for TransformOptions {
  type Target = TransformOptionsInner;

  fn deref(&self) -> &Self::Target {
    &self.inner
  }
}

impl DerefMut for TransformOptions {
  fn deref_mut(&mut self) -> &mut Self::Target {
    &mut self.inner
  }
}

impl TransformOptions {
  /// Create a new TransformOptions with Normal mode (pre-resolved OxcTransformOptions)
  #[inline]
  pub fn new(options: OxcTransformOptions, target: EngineTargets, jsx_preset: JsxPreset) -> Self {
    Self { inner: TransformOptionsInner::Normal(Arc::new(options)), target, jsx_preset }
  }

  /// Create a new TransformOptions with Raw mode (for Auto tsconfig lookup)
  #[inline]
  pub fn new_raw(
    bundler_options: BundlerTransformOptions,
    target: EngineTargets,
    jsx_preset: JsxPreset,
  ) -> Self {
    Self {
      inner: TransformOptionsInner::Raw((Arc::new(bundler_options), Arc::new(DashMap::new()))),
      target,
      jsx_preset,
    }
  }

  #[inline]
  pub fn is_jsx_disabled(&self) -> bool {
    matches!(self.jsx_preset, JsxPreset::Disable)
  }

  #[inline]
  pub fn is_jsx_preserve(&self) -> bool {
    matches!(self.jsx_preset, JsxPreset::Preserve)
  }

  pub fn should_transform_js(&self) -> bool {
    match &self.inner {
      TransformOptionsInner::Normal(opts) => opts.env.regexp.set_notation,
      TransformOptionsInner::Raw(_) => {
        // TODO: self.target.has_feature(..)
        false
      }
    }
  }

  /// Get OxcTransformOptions for a specific file (for Auto tsconfig mode)
  ///
  /// For Normal mode: returns the pre-resolved options
  /// For Raw mode: finds the nearest tsconfig, and either returns cached options or creates new ones
  ///
  /// # Parameters
  /// - `file_path`: The file to get transform options for
  /// - `create_fn`: Optional callback to create options if cache miss (only for Raw mode)
  pub fn options_for_file(&self, file_path: &Path) -> Arc<OxcTransformOptions> {
    match &self.inner {
      TransformOptionsInner::Normal(opts) => Arc::clone(opts),
      TransformOptionsInner::Raw((bundler_opts, cache)) => {
        // Find nearest tsconfig for this file
        let Some(tsconfig_path) = find_tsconfig_json_for_file(file_path) else {
          let cached = cache.entry(PathBuf::new()).or_insert_with(|| {
            // TODO: Handle warnings and errors
            Arc::new(
              merge_transform_options_with_tsconfig(
                bundler_opts.deref().clone(),
                None,
                &mut vec![],
              )
              .unwrap(),
            )
          });
          return Arc::clone(cached.value());
        };

        // Check cache first
        if let Some(cached) = cache.get(&tsconfig_path) {
          return Arc::clone(cached.value());
        }

        // TODO: handle resolver and tsconfig error
        let resolver = oxc_resolver::Resolver::default();
        let tsconfig = resolver.resolve_tsconfig(&tsconfig_path).unwrap();
        let resolved_tsconfig = tsconfig.resolve_for_file(file_path);

        // TODO: Handle warnings and errors
        let transform_options = Arc::new(
          merge_transform_options_with_tsconfig(
            bundler_opts.deref().clone(),
            Some(resolved_tsconfig.as_ref()),
            &mut vec![],
          )
          .unwrap(),
        );
        cache.insert(tsconfig_path, Arc::clone(&transform_options));
        transform_options
      }
    }
  }
}

impl Default for TransformOptions {
  fn default() -> Self {
    Self {
      inner: TransformOptionsInner::Normal(Arc::new(OxcTransformOptions::default())),
      target: EngineTargets::default(),
      jsx_preset: JsxPreset::default(),
    }
  }
}

/// Find the nearest tsconfig.json file for a given file path
/// Walks up the directory tree from the file's parent directory
/// Used in Auto tsconfig mode to find the appropriate tsconfig for each file
fn find_tsconfig_json_for_file(path: &Path) -> Option<PathBuf> {
  // don't load tsconfig for paths in node_modules like esbuild
  if path.components().any(|c| c.as_os_str() == "node_modules") {
    return None;
  }

  let mut dir = path.parent()?.to_path_buf();

  loop {
    let tsconfig_json = dir.join("tsconfig.json");
    if tsconfig_json.exists() {
      return Some(tsconfig_json);
    }

    let Some(parent) = dir.parent() else { break };
    dir = parent.to_path_buf();
  }

  None
}

pub fn merge_transform_options_with_tsconfig(
  mut transform_options: BundlerTransformOptions,
  tsconfig: Option<&oxc_resolver::TsConfig>,
  warnings: &mut Vec<BuildDiagnostic>,
) -> BuildResult<OxcTransformOptions> {
  if let Some(tsconfig) = &tsconfig {
    let compiler_options = &tsconfig.compiler_options;

    // when both the normal options and tsconfig is set, we want to prioritize the normal options
    if compiler_options.jsx.as_deref() == Some("preserve") {
      if transform_options
        .jsx
        .as_ref()
        .is_none_or(|jsx| matches!(jsx, Either::Right(right) if right.runtime.is_none()))
      {
        transform_options.jsx = Some(Either::Left(String::from("preserve")));
      } else {
        warnings.push(
          BuildDiagnostic::configuration_field_conflict(
            "transform",
            "jsx",
            "tsconfig.json",
            "compilerOptions.jsx",
          )
          .with_severity_warning(),
        );
      }
    }

    if !matches!(&transform_options.jsx, Some(Either::Left(left)) if left == "preserve") {
      let mut jsx = if let Some(Either::Right(jsx)) = transform_options.jsx {
        jsx
      } else {
        JsxOptions::default()
      };

      if compiler_options.jsx_factory.is_some() {
        if jsx.pragma.is_none() {
          jsx.pragma.clone_from(&compiler_options.jsx_factory);
        } else {
          warnings.push(
            BuildDiagnostic::configuration_field_conflict(
              "transform.jsx",
              "pragma",
              "tsconfig.json",
              "compilerOptions.jsxFactory",
            )
            .with_severity_warning(),
          );
        }
      }
      if compiler_options.jsx_import_source.is_some() {
        if jsx.import_source.is_none() {
          jsx.import_source.clone_from(&compiler_options.jsx_import_source);
        } else {
          warnings.push(
            BuildDiagnostic::configuration_field_conflict(
              "transform.jsx",
              "importSource",
              "tsconfig.json",
              "compilerOptions.jsxImportSource",
            )
            .with_severity_warning(),
          );
        }
      }
      if compiler_options.jsx_fragment_factory.is_some() {
        if jsx.pragma_frag.is_none() {
          jsx.pragma_frag.clone_from(&compiler_options.jsx_fragment_factory);
        } else {
          warnings.push(
            BuildDiagnostic::configuration_field_conflict(
              "transform.jsx",
              "pragmaFrag",
              "tsconfig.json",
              "compilerOptions.jsxFragmentFactory",
            )
            .with_severity_warning(),
          );
        }
      }

      if jsx.runtime.is_none() {
        match compiler_options.jsx.as_deref() {
          Some("react") => {
            jsx.runtime = Some(String::from("classic"));
            // this option should not be set when using classic runtime
            jsx.import_source = None;
          }
          Some("react-jsx") => {
            jsx.runtime = Some(String::from("automatic"));
            // these options should not be set when using automatic runtime
            jsx.pragma = None;
            jsx.pragma_frag = None;
          }
          Some("react-jsxdev") => jsx.development = Some(true),
          _ => {}
        }
      }

      transform_options.jsx = Some(Either::Right(jsx));
    }

    if transform_options.decorator.as_ref().is_none_or(|decorator| decorator.legacy.is_none()) {
      let mut decorator = transform_options.decorator.unwrap_or_default();

      if compiler_options.experimental_decorators.is_some() {
        decorator.legacy = compiler_options.experimental_decorators;
      }

      if compiler_options.emit_decorator_metadata.is_some() {
        decorator.emit_decorator_metadata = compiler_options.emit_decorator_metadata;
      }

      transform_options.decorator = Some(decorator);
    } else {
      if compiler_options.experimental_decorators.is_some() {
        warnings.push(
          BuildDiagnostic::configuration_field_conflict(
            "transform.decorator",
            "legacy",
            "tsconfig.json",
            "compilerOptions.experimentalDecorators",
          )
          .with_severity_warning(),
        );
      }
      if compiler_options.emit_decorator_metadata.is_some()
        && transform_options.decorator.as_ref().is_some_and(|d| d.emit_decorator_metadata.is_some())
      {
        warnings.push(
          BuildDiagnostic::configuration_field_conflict(
            "transform.decorator",
            "emitDecoratorMetadata",
            "tsconfig.json",
            "compilerOptions.emitDecoratorMetadata",
          )
          .with_severity_warning(),
        );
      }
    }

    // | preserveValueImports | importsNotUsedAsValues | verbatimModuleSyntax | onlyRemoveTypeImports |
    // | -------------------- | ---------------------- | -------------------- |---------------------- |
    // | false                | remove                 | false                | false                 |
    // | false                | preserve, error        | -                    | -                     |
    // | true                 | remove                 | -                    | -                     |
    // | true                 | preserve, error        | true                 | true                  |
    let mut typescript = transform_options.typescript.unwrap_or_default();
    if typescript.only_remove_type_imports.is_none() {
      if compiler_options.verbatim_module_syntax.is_some() {
        typescript.only_remove_type_imports = compiler_options.verbatim_module_syntax;
      } else if compiler_options.preserve_value_imports.is_some()
        || compiler_options.imports_not_used_as_values.is_some()
      {
        let preserve_value_imports = compiler_options.preserve_value_imports.unwrap_or(false);
        let imports_not_used_as_values =
          compiler_options.imports_not_used_as_values.as_deref().unwrap_or("remove");
        typescript.only_remove_type_imports =
          if !preserve_value_imports && imports_not_used_as_values == "remove" {
            Some(true)
          } else if preserve_value_imports
            && (imports_not_used_as_values == "preserve" || imports_not_used_as_values == "error")
          {
            Some(false)
          } else {
            // warnings.push(
            //   `preserveValueImports=${preserveValueImports} + importsNotUsedAsValues=${importsNotUsedAsValues} is not supported by oxc.` +
            //     'Please migrate to the new verbatimModuleSyntax option.',
            // )
            Some(false)
          };
      }
    } else if compiler_options.verbatim_module_syntax.is_some() {
      warnings.push(
        BuildDiagnostic::configuration_field_conflict(
          "transform.typescript",
          "onlyRemoveTypeImports",
          "tsconfig.json",
          "compilerOptions.verbatimModuleSyntax",
        )
        .with_severity_warning(),
      );
    }

    let disable_use_define_for_class_fields =
      !compiler_options.use_define_for_class_fields.unwrap_or_else(|| {
        let target = compiler_options.target.as_deref();
        let Some(target) = target else { return false };
        if target.len() < 3 || !&target[..2].eq_ignore_ascii_case("es") {
          return false;
        }
        let reset = &target[2..];
        if reset.eq_ignore_ascii_case("next") {
          return true;
        }
        reset.parse::<usize>().is_ok_and(|x| x > 2021)
      });

    let mut assumptions = transform_options.assumptions.unwrap_or_default();
    assumptions.set_public_class_fields = Some(disable_use_define_for_class_fields);
    typescript.remove_class_fields_without_initializer = Some(disable_use_define_for_class_fields);

    transform_options.typescript = Some(typescript);
    transform_options.assumptions = Some(assumptions);
  }

  Ok(transform_options.try_into().map_err(|message: String| {
    let hint = message
      .contains("Invalid target")
      .then(|| "Rolldown only supports ES2015 (ES6) and later.".to_owned());
    BuildDiagnostic::bundler_initialize_error(message, hint)
  })?)
}
