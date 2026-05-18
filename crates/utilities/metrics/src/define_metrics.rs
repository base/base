//! Macros for defining and describing metrics.

/// Defines a metrics struct named `Metrics` with static associated functions.
///
/// Each field becomes a function that returns the appropriate `metrics` handle
/// (or [`NoopMetric`] when the `metrics` feature is disabled).
///
/// The scope is prepended to every metric name with a dot separator.
/// The scope may contain dots (e.g., `my.app`) by using dot-separated idents.
/// To override the generated struct name, add `struct = MyMetrics,` after the
/// scope.
///
/// # Attributes
///
/// - `#[describe("...")]` — required per-field; human-readable description.
/// - `#[label(name)]` — optional per-field (may be repeated up to 2x).
/// - `#[label(name = "...")]` — optional per-field string label name.
/// - `#[label(name = "...", default = ["..."])]` — optional per-field labeled zero values.
/// - `#[no_zero]` — optional per-field; opts the metric out of the auto-generated
///   `Metrics::zero()` initialization. Use this for gauges that track external
///   state (e.g. account balances, chain heads) where `0` is a meaningful
///   alerting value and would generate false alerts during the warmup window
///   between process start and the first real read. The metric series will be
///   absent from scrapes until the first observation is recorded. The metric
///   is still registered with `describe()`.
///
/// # Example
///
/// ```ignore
/// base_metrics::define_metrics! {
///     my.app
///     #[describe("Total requests")]
///     requests_total: counter,
/// }
/// Metrics::requests_total().increment(1);
///
/// base_metrics::define_metrics! {
///     my.app,
///     struct = MyMetrics,
///     #[describe("Request duration")]
///     #[label(name = "method", default = ["GET", "POST"])]
///     request_duration: histogram,
/// }
/// MyMetrics::request_duration("GET").record(0.42);
/// ```
#[macro_export]
macro_rules! define_metrics {
    (
        $($scope:ident).+, struct = $name:ident,
        $(
            $(#[$($attr:tt)*])*
            $field:ident : $kind:ident
        ),*
        $(,)?
    ) => {
        $crate::__define_metrics_impl! {
            $name, {$($scope).+},
            $(
                $(#[$($attr)*])*
                $field : $kind
            ),*
        }
    };
    (
        $($scope:ident).+
        $(
            $(#[$($attr:tt)*])*
            $field:ident : $kind:ident
        ),*
        $(,)?
    ) => {
        $crate::__define_metrics_impl! {
            Metrics, {$($scope).+},
            $(
                $(#[$($attr)*])*
                $field : $kind
            ),*
        }
    };
}

/// Internal — builds the metrics struct after the scope has been packaged into
/// a brace group so it can be used inside field repetitions.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_metrics_impl {
    (
        $name:ident, $scope:tt,
        $(
            $(#[$($attr:tt)*])*
            $field:ident : $kind:ident
        ),*
        $(,)?
    ) => {
        /// Metrics accessor struct.
        pub struct $name;

        impl $name {
            $(
                $crate::__define_metric_fn!(
                    $scope, $field, $kind;
                    $(#[$($attr)*])*
                );
            )*

            /// Registers human-readable descriptions for all metrics.
            #[cfg(feature = "metrics")]
            pub fn describe() {
                $(
                    $crate::__describe_metric_from_attrs!($scope, $field, $kind; $(#[$($attr)*])*);
                )*
            }

            /// Initializes counters and gauges to zero so they appear immediately.
            #[cfg(feature = "metrics")]
            pub fn zero() {
                $(
                    $crate::__define_metric_zero!($field, $kind; $(#[$($attr)*])*);
                )*
            }

            /// Registers descriptions and initializes counters and gauges.
            #[cfg(feature = "metrics")]
            pub fn init() {
                Self::describe();
                Self::zero();
            }

            /// No-op when the `metrics` feature is disabled.
            #[cfg(not(feature = "metrics"))]
            #[inline(always)]
            pub fn describe() {}

            /// No-op when the `metrics` feature is disabled.
            #[cfg(not(feature = "metrics"))]
            #[inline(always)]
            pub fn zero() {}

            /// No-op when the `metrics` feature is disabled.
            #[cfg(not(feature = "metrics"))]
            #[inline(always)]
            pub fn init() {}
        }

        #[cfg(feature = "metrics")]
        const _: () = {
            #[$crate::__private_ctor::ctor(anonymous, crate_path = $crate::__private_ctor)]
            fn register_metrics_initializer() {
                $crate::register_initializer($name::init);
            }
        };
    };
}

/// Internal — generates a single metric accessor function.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_metric_fn {
    ($scope:tt, $field:ident, $kind:ident; $($attrs:tt)*) => {
        $crate::__metric_label_keys!(@collect ($scope, $field, $kind) [] $($attrs)*);
    };
    (@emit $macro_name:ident $ret:ident @fn2 {$($scope:ident).+}, $field:ident, $l1:expr, $l2:expr) => {
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(feature = "metrics")]
        #[allow(unused)]
        pub fn $field(
            label0: impl Into<::metrics::SharedString>,
            label1: impl Into<::metrics::SharedString>,
        ) -> ::metrics::$ret {
            ::metrics::$macro_name!(
                concat!($(stringify!($scope), ".",)+ stringify!($field)),
                $l1 => label0,
                $l2 => label1
            )
        }
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(not(feature = "metrics"))]
        #[inline(always)]
        #[allow(unused)]
        pub fn $field<S1, S2>(_: S1, _: S2) -> $crate::NoopMetric { $crate::NoopMetric }
    };
    (@emit $macro_name:ident $ret:ident @fn1 {$($scope:ident).+}, $field:ident, $l:expr) => {
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(feature = "metrics")]
        #[allow(unused)]
        pub fn $field(label0: impl Into<::metrics::SharedString>) -> ::metrics::$ret {
            ::metrics::$macro_name!(concat!($(stringify!($scope), ".",)+ stringify!($field)), $l => label0)
        }
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(not(feature = "metrics"))]
        #[inline(always)]
        #[allow(unused)]
        pub fn $field<S>(_: S) -> $crate::NoopMetric { $crate::NoopMetric }
    };
    (@emit $macro_name:ident $ret:ident @fn0 {$($scope:ident).+}, $field:ident) => {
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(feature = "metrics")]
        #[allow(unused)]
        pub fn $field() -> ::metrics::$ret {
            ::metrics::$macro_name!(concat!($(stringify!($scope), ".",)+ stringify!($field)))
        }
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(not(feature = "metrics"))]
        #[inline(always)]
        #[allow(unused)]
        pub fn $field() -> $crate::NoopMetric { $crate::NoopMetric }
    };
}

/// Internal — parses label attributes into accessor function label keys.
#[doc(hidden)]
#[macro_export]
macro_rules! __metric_label_keys {
    (@collect ($scope:tt, $field:ident, $kind:ident) [$($labels:expr,)*] #[describe($desc:expr)] $($rest:tt)*) => {
        $crate::__metric_label_keys!(@collect ($scope, $field, $kind) [$($labels,)*] $($rest)*);
    };
    (@collect ($scope:tt, $field:ident, $kind:ident) [$($labels:expr,)*] #[no_zero] $($rest:tt)*) => {
        $crate::__metric_label_keys!(@collect ($scope, $field, $kind) [$($labels,)*] $($rest)*);
    };
    (@collect ($scope:tt, $field:ident, $kind:ident) [$($labels:expr,)*] #[label($label:ident)] $($rest:tt)*) => {
        $crate::__metric_label_keys!(@collect ($scope, $field, $kind) [$($labels,)* stringify!($label),] $($rest)*);
    };
    (@collect ($scope:tt, $field:ident, $kind:ident) [$($labels:expr,)*] #[label(name = $label_name:expr)] $($rest:tt)*) => {
        $crate::__metric_label_keys!(@collect ($scope, $field, $kind) [$($labels,)* $label_name,] $($rest)*);
    };
    (@collect ($scope:tt, $field:ident, $kind:ident) [$($labels:expr,)*] #[label(name = $label_name:expr, default = [$($default:expr),* $(,)?])] $($rest:tt)*) => {
        $crate::__metric_label_keys!(@collect ($scope, $field, $kind) [$($labels,)* $label_name,] $($rest)*);
    };
    (@collect ($scope:tt, $field:ident, counter) [] ) => {
        $crate::__define_metric_fn!(@emit counter Counter @fn0 $scope, $field);
    };
    (@collect ($scope:tt, $field:ident, gauge) [] ) => {
        $crate::__define_metric_fn!(@emit gauge Gauge @fn0 $scope, $field);
    };
    (@collect ($scope:tt, $field:ident, histogram) [] ) => {
        $crate::__define_metric_fn!(@emit histogram Histogram @fn0 $scope, $field);
    };
    (@collect ($scope:tt, $field:ident, counter) [$l1:expr,] ) => {
        $crate::__define_metric_fn!(@emit counter Counter @fn1 $scope, $field, $l1);
    };
    (@collect ($scope:tt, $field:ident, gauge) [$l1:expr,] ) => {
        $crate::__define_metric_fn!(@emit gauge Gauge @fn1 $scope, $field, $l1);
    };
    (@collect ($scope:tt, $field:ident, histogram) [$l1:expr,] ) => {
        $crate::__define_metric_fn!(@emit histogram Histogram @fn1 $scope, $field, $l1);
    };
    (@collect ($scope:tt, $field:ident, counter) [$l1:expr, $l2:expr,] ) => {
        $crate::__define_metric_fn!(@emit counter Counter @fn2 $scope, $field, $l1, $l2);
    };
    (@collect ($scope:tt, $field:ident, gauge) [$l1:expr, $l2:expr,] ) => {
        $crate::__define_metric_fn!(@emit gauge Gauge @fn2 $scope, $field, $l1, $l2);
    };
    (@collect ($scope:tt, $field:ident, histogram) [$l1:expr, $l2:expr,] ) => {
        $crate::__define_metric_fn!(@emit histogram Histogram @fn2 $scope, $field, $l1, $l2);
    };
    (@collect ($scope:tt, $field:ident, $kind:ident) [$l1:expr, $l2:expr, $l3:expr $(, $rest:expr)*,] ) => {
        ::core::compile_error!("define_metrics! supports at most 2 #[label(...)] attributes per metric");
    };
}

/// Internal — emits a `metrics::describe_*!` call.
#[doc(hidden)]
#[macro_export]
macro_rules! __describe_metric {
    ({$($scope:ident).+}, $field:ident, counter, $desc:expr) => {
        ::metrics::describe_counter!(concat!($(stringify!($scope), ".",)+ stringify!($field)), $desc);
    };
    ({$($scope:ident).+}, $field:ident, gauge, $desc:expr) => {
        ::metrics::describe_gauge!(concat!($(stringify!($scope), ".",)+ stringify!($field)), $desc);
    };
    ({$($scope:ident).+}, $field:ident, histogram, $desc:expr) => {
        ::metrics::describe_histogram!(concat!($(stringify!($scope), ".",)+ stringify!($field)), $desc);
    };
}

/// Internal — extracts a metric description from its attributes.
#[doc(hidden)]
#[macro_export]
macro_rules! __describe_metric_from_attrs {
    ($scope:tt, $field:ident, $kind:ident; #[describe($desc:expr)] $($rest:tt)*) => {
        $crate::__describe_metric!($scope, $field, $kind, $desc);
    };
    ($scope:tt, $field:ident, $kind:ident; #[$($attr:tt)*] $($rest:tt)*) => {
        $crate::__describe_metric_from_attrs!($scope, $field, $kind; $($rest)*);
    };
    ($scope:tt, $field:ident, $kind:ident; ) => {
        ::core::compile_error!("define_metrics! fields require a #[describe(\"...\")] attribute");
    };
}

/// Internal — emits zeroing code for counters and gauges.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_metric_zero {
    ($field:ident, $kind:ident; $($attrs:tt)*) => {
        $crate::__metric_zero_defaults!(@collect ($field, $kind) [] $($attrs)*);
    };
}

/// Internal — parses zero defaults from label attributes.
#[doc(hidden)]
#[macro_export]
macro_rules! __metric_zero_defaults {
    // `#[no_zero]` short-circuits the entire chain: consume any remaining
    // attributes and emit no zeroing code for this field.
    (@collect ($field:ident, $kind:ident) [$($labels:tt)*] #[no_zero] $($rest:tt)*) => {};
    (@collect ($field:ident, $kind:ident) [$($labels:tt)*] #[describe($desc:expr)] $($rest:tt)*) => {
        $crate::__metric_zero_defaults!(@collect ($field, $kind) [$($labels)*] $($rest)*);
    };
    (@collect ($field:ident, $kind:ident) [$($labels:tt)*] #[label($label:ident)] $($rest:tt)*) => {
        $crate::__metric_zero_defaults!(@collect ($field, $kind) [$($labels)* (@none)] $($rest)*);
    };
    (@collect ($field:ident, $kind:ident) [$($labels:tt)*] #[label(name = $label_name:expr)] $($rest:tt)*) => {
        $crate::__metric_zero_defaults!(@collect ($field, $kind) [$($labels)* (@none)] $($rest)*);
    };
    (@collect ($field:ident, $kind:ident) [$($labels:tt)*] #[label(name = $label_name:expr, default = [$($default:expr),* $(,)?])] $($rest:tt)*) => {
        $crate::__metric_zero_defaults!(@collect ($field, $kind) [$($labels)* (@defaults [$($default),*])] $($rest)*);
    };
    (@collect ($field:ident, counter) [] ) => {
        Self::$field().absolute(0);
    };
    (@collect ($field:ident, gauge) [] ) => {
        Self::$field().set(0.0);
    };
    (@collect ($field:ident, histogram) [] ) => {};
    (@collect ($field:ident, $kind:ident) [(@defaults [$($d1:expr),*])] ) => {
        {
            let label_values = [$(::metrics::SharedString::from($d1)),*];
            for label in label_values {
                $crate::__zero_metric_call_1!($field, $kind, label);
            }
        }
    };
    (@collect ($field:ident, $kind:ident) [(@defaults [$($d1:expr),*]) (@defaults [$($d2:expr),*])] ) => {
        {
            let label1_values = [$(::metrics::SharedString::from($d1)),*];
            let label2_values = [$(::metrics::SharedString::from($d2)),*];
            for label1 in &label1_values {
                for label2 in &label2_values {
                    $crate::__zero_metric_call_2!($field, $kind, label1.clone(), label2.clone());
                }
            }
        }
    };
    // Mixed default/non-default labeled metrics are intentionally left uninitialized.
    (@collect ($field:ident, $kind:ident) [$($labels:tt)+] ) => {};
}

/// Internal — zeroes a single-label metric.
#[doc(hidden)]
#[macro_export]
macro_rules! __zero_metric_call_1 {
    ($field:ident, counter, $label:expr) => {
        Self::$field($label).absolute(0);
    };
    ($field:ident, gauge, $label:expr) => {
        Self::$field($label).set(0.0);
    };
    ($field:ident, histogram, $label:expr) => {};
}

/// Internal — zeroes a two-label metric.
#[doc(hidden)]
#[macro_export]
macro_rules! __zero_metric_call_2 {
    ($field:ident, counter, $label1:expr, $label2:expr) => {
        Self::$field($label1, $label2).absolute(0);
    };
    ($field:ident, gauge, $label1:expr, $label2:expr) => {
        Self::$field($label1, $label2).set(0.0);
    };
    ($field:ident, histogram, $label1:expr, $label2:expr) => {};
}
