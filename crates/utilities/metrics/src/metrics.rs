//! Macros for defining and describing metrics.

/// Defines a metrics struct named `Metrics` with static associated functions.
///
/// Each field becomes a function that returns the appropriate `metrics` handle
/// (or [`NoopMetric`] when the `metrics` feature is disabled).
///
/// The scope ident is prepended to every metric name with a dot separator.
/// For a custom struct name, use [`define_metrics_struct!`].
///
/// # Attributes
///
/// - `#[describe("...")]` — required per-field; human-readable description.
/// - `#[label(name)]` — optional per-field (may be repeated up to 2x).
///
/// # Example
///
/// ```ignore
/// base_metrics::define_metrics! {
///     my_app
///     #[describe("Total requests")]
///     requests_total: counter,
/// }
/// Metrics::requests_total().increment(1);
/// ```
#[macro_export]
macro_rules! define_metrics {
    (
        $scope:ident
        $(
            #[describe($desc:expr)]
            $(#[label($label:ident)])*
            $field:ident : $kind:ident
        ),*
        $(,)?
    ) => {
        $crate::define_metrics_struct! {
            Metrics, $scope,
            $(
                #[describe($desc)]
                $(#[label($label)])*
                $field : $kind
            ),*
        }
    };
}

/// Like [`define_metrics!`] but with a custom struct name.
///
/// # Example
///
/// ```ignore
/// base_metrics::define_metrics_struct! {
///     MyMetrics, my_app,
///     #[describe("Request duration")]
///     #[label(method)]
///     request_duration: histogram,
/// }
/// MyMetrics::request_duration("GET").record(0.42);
/// ```
#[macro_export]
macro_rules! define_metrics_struct {
    (
        $name:ident, $scope:ident,
        $(
            #[describe($desc:expr)]
            $(#[label($label:ident)])*
            $field:ident : $kind:ident
        ),*
        $(,)?
    ) => {
        /// Metrics accessor struct.
        pub struct $name;

        impl $name {
            $(
                $crate::__define_metric_fn!(
                    $scope, $field, $kind
                    $(; label = $label)*
                );
            )*

            /// Registers human-readable descriptions for all metrics.
            #[cfg(feature = "metrics")]
            pub fn describe() {
                $(
                    $crate::__describe_metric!($scope, $field, $kind, $desc);
                )*
            }

            /// No-op when the `metrics` feature is disabled.
            #[cfg(not(feature = "metrics"))]
            #[inline(always)]
            pub fn describe() {}
        }
    };
}

/// Internal — generates a single metric accessor function.
#[doc(hidden)]
#[macro_export]
macro_rules! __define_metric_fn {
    ($scope:ident, $field:ident, counter; label = $l1:ident; label = $l2:ident) => {
        $crate::__define_metric_fn!(@emit counter Counter @fn2 $scope, $field, $l1, $l2);
    };
    ($scope:ident, $field:ident, gauge; label = $l1:ident; label = $l2:ident) => {
        $crate::__define_metric_fn!(@emit gauge Gauge @fn2 $scope, $field, $l1, $l2);
    };
    ($scope:ident, $field:ident, histogram; label = $l1:ident; label = $l2:ident) => {
        $crate::__define_metric_fn!(@emit histogram Histogram @fn2 $scope, $field, $l1, $l2);
    };
    ($scope:ident, $field:ident, counter; label = $l:ident) => {
        $crate::__define_metric_fn!(@emit counter Counter @fn1 $scope, $field, $l);
    };
    ($scope:ident, $field:ident, gauge; label = $l:ident) => {
        $crate::__define_metric_fn!(@emit gauge Gauge @fn1 $scope, $field, $l);
    };
    ($scope:ident, $field:ident, histogram; label = $l:ident) => {
        $crate::__define_metric_fn!(@emit histogram Histogram @fn1 $scope, $field, $l);
    };
    ($scope:ident, $field:ident, counter) => {
        $crate::__define_metric_fn!(@emit counter Counter @fn0 $scope, $field);
    };
    ($scope:ident, $field:ident, gauge) => {
        $crate::__define_metric_fn!(@emit gauge Gauge @fn0 $scope, $field);
    };
    ($scope:ident, $field:ident, histogram) => {
        $crate::__define_metric_fn!(@emit histogram Histogram @fn0 $scope, $field);
    };
    (@emit $macro_name:ident $ret:ident @fn2 $scope:ident, $field:ident, $l1:ident, $l2:ident) => {
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(feature = "metrics")]
        #[allow(unused)]
        pub fn $field($l1: impl Into<::metrics::SharedString>, $l2: impl Into<::metrics::SharedString>) -> ::metrics::$ret {
            ::metrics::$macro_name!(concat!(stringify!($scope), ".", stringify!($field)), stringify!($l1) => $l1, stringify!($l2) => $l2)
        }
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(not(feature = "metrics"))]
        #[inline(always)]
        #[allow(unused)]
        pub fn $field<S1, S2>(_: S1, _: S2) -> $crate::NoopMetric { $crate::NoopMetric }
    };
    (@emit $macro_name:ident $ret:ident @fn1 $scope:ident, $field:ident, $l:ident) => {
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(feature = "metrics")]
        #[allow(unused)]
        pub fn $field($l: impl Into<::metrics::SharedString>) -> ::metrics::$ret {
            ::metrics::$macro_name!(concat!(stringify!($scope), ".", stringify!($field)), stringify!($l) => $l)
        }
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(not(feature = "metrics"))]
        #[inline(always)]
        #[allow(unused)]
        pub fn $field<S>(_: S) -> $crate::NoopMetric { $crate::NoopMetric }
    };
    (@emit $macro_name:ident $ret:ident @fn0 $scope:ident, $field:ident) => {
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(feature = "metrics")]
        #[allow(unused)]
        pub fn $field() -> ::metrics::$ret {
            ::metrics::$macro_name!(concat!(stringify!($scope), ".", stringify!($field)))
        }
        #[doc = concat!("Returns the `", stringify!($field), "` ", stringify!($macro_name), ".")]
        #[cfg(not(feature = "metrics"))]
        #[inline(always)]
        #[allow(unused)]
        pub fn $field() -> $crate::NoopMetric { $crate::NoopMetric }
    };
}

/// Internal — emits a `metrics::describe_*!` call.
#[doc(hidden)]
#[macro_export]
macro_rules! __describe_metric {
    ($scope:ident, $field:ident, counter, $desc:expr) => {
        ::metrics::describe_counter!(concat!(stringify!($scope), ".", stringify!($field)), $desc);
    };
    ($scope:ident, $field:ident, gauge, $desc:expr) => {
        ::metrics::describe_gauge!(concat!(stringify!($scope), ".", stringify!($field)), $desc);
    };
    ($scope:ident, $field:ident, histogram, $desc:expr) => {
        ::metrics::describe_histogram!(concat!(stringify!($scope), ".", stringify!($field)), $desc);
    };
}

/// Creates a [`DropTimer`] (or [`NoopDropTimer`]) that records elapsed duration
/// to a histogram metric on drop.
///
/// # Examples
///
/// ```ignore
/// let _timer = base_metrics::timed!(Metrics::proof_duration_seconds());
///
/// let mut timer = base_metrics::timed!(Metrics::witness_build_duration_seconds());
/// timer.stop();
/// ```
#[macro_export]
macro_rules! timed {
    ($metric_handle:expr) => {{
        #[cfg(feature = "metrics")]
        {
            $crate::DropTimer::new($metric_handle)
        }
        #[cfg(not(feature = "metrics"))]
        {
            let _ = &$metric_handle;
            $crate::NoopDropTimer
        }
    }};
}

/// Executes a block and records its duration to a histogram metric.
///
/// Returns the value of the block expression.
///
/// # Examples
///
/// ```ignore
/// let result = base_metrics::time!(Metrics::request_duration(), {
///     do_work().await
/// });
/// ```
#[macro_export]
macro_rules! time {
    ($metric_handle:expr, $body:block) => {{
        let mut __timer = $crate::timed!($metric_handle);
        let __result = $body;
        __timer.stop();
        __result
    }};
}

/// Sets a metric value, optionally with a specified label.
#[macro_export]
macro_rules! set {
    (counter, $metric:path, $key:expr, $value:expr, $amount:expr) => {
        #[cfg(feature = "metrics")]
        metrics::counter!($metric, $key => $value).absolute($amount);
    };
    ($instrument:ident, $metric:path, $key:expr, $value:expr, $amount:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric, $key => $value).set($amount);
    };
    (counter, $metric:path, $value:expr, $amount:expr) => {
        #[cfg(feature = "metrics")]
        metrics::counter!($metric, "type" => $value).absolute($amount);
    };
    ($instrument:ident, $metric:path, $value:expr, $amount:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric, "type" => $value).set($amount);
    };
    (counter, $metric:path, $value:expr) => {
        #[cfg(feature = "metrics")]
        metrics::counter!($metric).absolute($value);
    };
    ($instrument:ident, $metric:path, $value:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric).set($value);
    };
}

/// Increments a metric value, optionally with a specified label.
#[macro_export]
macro_rules! inc {
    ($instrument:ident, $metric:path, $value:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric, "type" => $value).increment(1);
    };
    ($instrument:ident, $metric:path $(, $label_key:expr $(=> $label_value:expr)?)*$(,)?) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric $(, $label_key $(=> $label_value)?)*).increment(1);
    };
    ($instrument:ident, $metric:path, $value:expr $(, $label_key:expr $(=> $label_value:expr)?)*$(,)?) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric $(, $label_key $(=> $label_value)?)*).increment($value);
    };
}

/// Decrements a metric value, optionally with a specified label.
#[macro_export]
macro_rules! dec {
    ($instrument:ident, $metric:path, $value:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric, "type" => $value).decrement(1.0);
    };
    ($instrument:ident, $metric:path $(, $label_key:expr $(=> $label_value:expr)?)*$(,)?) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric $(, $label_key $(=> $label_value)?)*).decrement(1.0);
    };
    ($instrument:ident, $metric:path, $value:expr $(, $label_key:expr $(=> $label_value:expr)?)*$(,)?) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric $(, $label_key $(=> $label_value)?)*).decrement($value);
    };
}

/// Records a value, optionally with a specified label.
#[macro_export]
macro_rules! record {
    ($instrument:ident, $metric:path, $key:expr, $value:expr, $amount:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric, $key => $value).record($amount);
    };
    ($instrument:ident, $metric:path, $amount:expr) => {
        #[cfg(feature = "metrics")]
        metrics::$instrument!($metric).record($amount);
    };
}
