/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::fmt;
use std::fmt::Display;

use allocative::Allocative;
use buck2_core::provider::label::ConfiguredProvidersLabel;
use buck2_core::provider::label::ProviderName;
use buck2_interpreter::types::label::Label;
use starlark::any::ProvidesStaticType;
use starlark::coerce::Coerce;
use starlark::environment::Methods;
use starlark::environment::MethodsBuilder;
use starlark::environment::MethodsStatic;
use starlark::values::Freeze;
use starlark::values::Heap;
use starlark::values::NoSerialize;
use starlark::values::StarlarkValue;
use starlark::values::Trace;
use starlark::values::Value;
use starlark::values::ValueLike;
use thiserror::Error;

use crate::interpreter::rule_defs::provider::ProviderCollection;

#[derive(Debug, Error)]
enum DependencyError {
    #[error("Unknown subtarget, could not find `{0}`")]
    UnknownSubtarget(String),
}

/// Wraps a dependency's `ProvidersLabel` and the result of analysis together for users' rule implementation functions
///
/// From Starlark, the label is accessible with `.label`, and providers from the underlying
/// `ProviderCollection` are available via `[]` (`get()`)
#[derive(
    Debug,
    Trace,
    Coerce,
    Freeze,
    ProvidesStaticType,
    NoSerialize,
    Allocative
)]
#[repr(C)]
pub struct DependencyGen<V> {
    label: V,
    providers_collection: V,
}

starlark_complex_value!(pub Dependency);

impl<V: Display> Display for DependencyGen<V> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "<dependency ")?;
        self.label.fmt(f)?;
        write!(f, ">")
    }
}

impl<'v> Dependency<'v> {
    pub fn new(
        heap: &'v Heap,
        label: ConfiguredProvidersLabel,
        providers_collection: Value<'v>,
    ) -> Self {
        Dependency {
            label: heap.alloc(Label::new(label)),
            providers_collection,
        }
    }

    pub fn label(&self) -> &Label {
        Label::from_value(self.label).unwrap()
    }

    fn provider_collection(&self) -> anyhow::Result<&ProviderCollection<'v>> {
        ProviderCollection::from_value(self.providers_collection)
            .ok_or_else(|| anyhow::anyhow!("internal error: not a ProviderCollection"))
    }
}

impl<'v, V: ValueLike<'v> + 'v> StarlarkValue<'v> for DependencyGen<V>
where
    Self: ProvidesStaticType,
{
    starlark_type!("dependency");

    fn get_methods() -> Option<&'static Methods> {
        static RES: MethodsStatic = MethodsStatic::new();
        RES.methods(dependency_functions)
    }

    fn at(&self, index: Value<'v>, heap: &'v Heap) -> anyhow::Result<Value<'v>> {
        self.providers_collection.to_value().at(index, heap)
    }

    fn is_in(&self, other: Value<'v>) -> anyhow::Result<bool> {
        self.providers_collection.to_value().is_in(other)
    }
}

#[starlark_module]
fn dependency_functions(builder: &mut MethodsBuilder) {
    #[starlark(attribute)]
    fn label<'v>(this: &Dependency) -> anyhow::Result<Value<'v>> {
        Ok(this.label.to_value())
    }

    // TODO(nga): should return provider collection.
    #[starlark(attribute)]
    fn providers<'v>(this: &Dependency) -> anyhow::Result<Vec<Value<'v>>> {
        Ok(this
            .provider_collection()?
            .providers
            .values()
            .copied()
            .collect())
    }

    /// Obtain the dependency representing a subtarget. In most cases you will want to use
    /// `x[DefaultInfo].sub_targets["foo"]` to get the _providers_ of the subtarget, but if you
    /// need a real `"dependency"` type (e.g. for use with `ctx.action.anon_target`) then use
    /// this method.
    fn sub_target<'v>(
        this: &Dependency<'v>,
        #[starlark(require = pos)] subtarget: &str,
        heap: &'v Heap,
    ) -> anyhow::Result<Dependency<'v>> {
        let di = this.provider_collection()?.default_info();
        let providers = di
            .get_sub_target_providers(subtarget)
            .ok_or_else(|| DependencyError::UnknownSubtarget(subtarget.to_owned()))?;
        let lbl = Label::from_value(this.label).unwrap().inner();
        let lbl = ConfiguredProvidersLabel::new(
            lbl.target().clone(),
            lbl.name().push(ProviderName::new(subtarget.to_owned())?),
        );
        Ok(Dependency::new(heap, lbl, providers.to_value()))
    }

    fn get<'v>(this: &Dependency<'v>, index: Value<'v>) -> anyhow::Result<Value<'v>> {
        this.provider_collection()?.get(index)
    }
}

#[cfg(test)]
mod tests {
    use buck2_common::result::SharedResult;
    use buck2_core::configuration::data::ConfigurationData;
    use buck2_core::pattern::pattern_type::ProvidersPatternExtra;
    use buck2_core::pattern::ParsedPattern;
    use buck2_interpreter_for_build::interpreter::build_context::BuildContext;
    use buck2_interpreter_for_build::interpreter::testing::Tester;
    use indoc::indoc;
    use starlark::environment::GlobalsBuilder;
    use starlark::eval::Evaluator;
    use starlark::values::Value;

    use crate::interpreter::rule_defs::provider::dependency::Dependency;
    use crate::interpreter::rule_defs::provider::ProviderCollection;

    #[starlark_module]
    fn dependency_creator(builder: &mut GlobalsBuilder) {
        fn create_collection<'v>(
            s: &str,
            providers: Value<'v>,
            eval: &mut Evaluator<'v, '_>,
        ) -> anyhow::Result<Dependency<'v>> {
            let c = BuildContext::from_context(eval)?;
            let label = match ParsedPattern::<ProvidersPatternExtra>::parse_precise(
                s,
                c.cell_info().name().name(),
                c.cell_info().cell_resolver(),
            ) {
                Ok(ParsedPattern::Target(package, target_name, providers)) => providers
                    .into_providers_label(package, target_name.as_ref())
                    .configure(ConfigurationData::testing_new()),
                _ => {
                    eprintln!("Expected a target, not {}", s);
                    panic!();
                }
            };
            let collection = eval
                .heap()
                .alloc(ProviderCollection::try_from_value(providers)?);

            Ok(Dependency::new(eval.heap(), label, collection))
        }
    }

    #[test]
    fn dependency_works() -> SharedResult<()> {
        let mut tester = Tester::new()?;
        tester.additional_globals(crate::interpreter::rule_defs::register_rule_defs);
        tester.additional_globals(dependency_creator);
        tester.run_starlark_bzl_test(indoc!(
            r#"
            frozen = create_collection("root//foo:bar[baz]", [DefaultInfo()])
            def test():
                notfrozen = create_collection("root//foo:bar[baz]", [DefaultInfo()])
                expect = "<dependency root//foo:bar[baz] (<testing>#<HASH>)>"

                assert_eq_ignore_hash(expect, repr(notfrozen))
                assert_eq({}, notfrozen[DefaultInfo].sub_targets)
                assert_eq(["baz"], notfrozen.label.sub_target)

                assert_eq_ignore_hash(expect, repr(frozen))
                assert_eq({}, frozen[DefaultInfo].sub_targets)
                assert_eq(["baz"], frozen.label.sub_target)
            "#
        ))?;
        Ok(())
    }
}
