use crate::{
    event::metric::{Metric, MetricKind, MetricValue, Sample},
    sinks::util::batch::{Batch, BatchConfig, BatchError, BatchSettings, BatchSize, PushResult},
    Event,
};
use std::{
    cmp::Ordering,
    collections::HashSet,
    hash::{Hash, Hasher},
    mem::discriminant,
    ops::{Deref, DerefMut},
};

#[derive(Clone, Debug)]
pub struct MetricEntry(pub Metric);

impl Eq for MetricEntry {}

impl Hash for MetricEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let metric = &self.0;
        metric.series.hash(state);
        metric.data.kind.hash(state);
        discriminant(&metric.data.value).hash(state);

        match &metric.data.value {
            MetricValue::AggregatedHistogram { buckets, .. } => {
                for bucket in buckets {
                    bucket.upper_limit.to_bits().hash(state);
                }
            }
            MetricValue::AggregatedSummary { quantiles, .. } => {
                for quantile in quantiles {
                    quantile.upper_limit.to_bits().hash(state);
                }
            }
            _ => {}
        }
    }
}

impl PartialEq for MetricEntry {
    fn eq(&self, other: &Self) -> bool {
        // This differs from a straightforward implementation of `eq` by
        // comparing only the "shape" bits (name, tags, and type) while
        // allowing the contained values to be different.
        self.series == other.series
            && self.data.kind == other.data.kind
            && discriminant(&self.data.value) == discriminant(&other.data.value)
            && match (&self.data.value, &other.data.value) {
                (
                    MetricValue::AggregatedHistogram {
                        buckets: buckets1, ..
                    },
                    MetricValue::AggregatedHistogram {
                        buckets: buckets2, ..
                    },
                ) => {
                    buckets1.len() == buckets2.len()
                        && buckets1
                            .iter()
                            .zip(buckets2.iter())
                            .all(|(b1, b2)| b1.upper_limit == b2.upper_limit)
                }
                (
                    MetricValue::AggregatedSummary {
                        quantiles: quantiles1,
                        ..
                    },
                    MetricValue::AggregatedSummary {
                        quantiles: quantiles2,
                        ..
                    },
                ) => {
                    quantiles1.len() == quantiles2.len()
                        && quantiles1
                            .iter()
                            .zip(quantiles2.iter())
                            .all(|(q1, q2)| q1.upper_limit == q2.upper_limit)
                }
                _ => true,
            }
    }
}

impl Deref for MetricEntry {
    type Target = Metric;
    fn deref(&self) -> &Metric {
        &self.0
    }
}

impl DerefMut for MetricEntry {
    fn deref_mut(&mut self) -> &mut Metric {
        &mut self.0
    }
}

pub type MetricBuffer = MetricsBuffer<StdMetricsState>;

/// The metrics buffer is a data structure for collecting a flow of data
/// points into a batch.
///
/// Batching mostly means that we will aggregate away timestamp
/// information, and apply metric-specific compression to improve the
/// performance of the pipeline.  In particular, only the latest in a
/// series of metrics are output.  Any aggregation or conversion of
/// metrics is handled by the `State` type.  Further, distribution
/// metrics have their their samples compressed with
/// `compress_distribution` below.
pub struct MetricsBuffer<State> {
    state: State,
    metrics: MetricSet,
    max_events: usize,
}

impl<State: MetricsState> MetricsBuffer<State> {
    pub fn new(settings: BatchSize<Self>) -> Self {
        Self::new_with_state(settings.events, State::default())
    }

    fn new_with_state(max_events: usize, state: State) -> Self {
        Self {
            state,
            metrics: MetricSet::with_capacity(max_events),
            max_events,
        }
    }
}

impl<State: MetricsState> Batch for MetricsBuffer<State> {
    type Input = Event;
    type Output = Vec<Metric>;

    fn get_settings_defaults(
        config: BatchConfig,
        defaults: BatchSettings<Self>,
    ) -> Result<BatchSettings<Self>, BatchError> {
        Ok(config
            .disallow_max_bytes()?
            .use_size_as_events()?
            .get_settings_or_default(defaults))
    }

    fn push(&mut self, item: Self::Input) -> PushResult<Self::Input> {
        if self.num_items() >= self.max_events {
            PushResult::Overflow(item)
        } else {
            if let Some(item) = self.state.apply_state(item.into_metric()) {
                self.metrics.replace(MetricEntry(item));
            }
            PushResult::Ok(self.num_items() >= self.max_events)
        }
    }

    fn is_empty(&self) -> bool {
        self.num_items() == 0
    }

    fn fresh(&self) -> Self {
        Self::new_with_state(self.max_events, self.state.fresh(&self.metrics))
    }

    fn finish(self) -> Self::Output {
        self.metrics
            .0
            .into_iter()
            .map(|e| {
                let mut metric = e.0;
                if let MetricValue::Distribution { samples, statistic } = metric.data.value {
                    let samples = compress_distribution(samples);
                    metric.data.value = MetricValue::Distribution { samples, statistic };
                };
                metric
            })
            .collect()
    }

    fn num_items(&self) -> usize {
        self.metrics.len()
    }
}

/// The metrics state trait abstracts how data point normalization is
/// done.  Normalisation is required to make sure Sources and Sinks are
/// exchanging compatible data structures. For instance, delta gauges
/// produced by Statsd source cannot be directly sent to Datadog API. In
/// this case the buffer will keep the state of a gauge value, and
/// produce absolute values gauges that are well supported by Datadog.
///
/// Another example of normalisation is disaggregation of counters. Most
/// sinks would expect we send them delta counters (e.g. how many events
/// occurred during the flush period). And most sources are producing
/// exactly these kind of counters, with Prometheus being a notable
/// exception. If the counter comes already aggregated inside the
/// source, the buffer will compare it's values with the previous known
/// and calculate the delta.
pub trait MetricsState: Default {
    fn apply_state(&mut self, metric: Metric) -> Option<Metric>;
    fn fresh(&self, metrics: &MetricSet) -> Self;
}

/// This is the "standard" metrics normalization handler. It handles two cases:
///
/// 1. Absolute counters are disaggregated into incremental counters,
/// indicating how many events happened during the flush period.
///
/// 2. Incremental gauges are converted into absolute values by keeping
/// track of the accumulated value and re-emitting the resulting value
/// as an absolute gauge.
///
/// All other metrics are left unchanged.
#[derive(Default)]
pub struct StdMetricsState {
    state: MetricSet,
}

impl MetricsState for StdMetricsState {
    fn apply_state(&mut self, metric: Metric) -> Option<Metric> {
        match (metric.data.kind, &metric.data.value) {
            // Counters are disaggregated. We take the previous value from the state
            // and emit the difference between previous and current as a Counter
            (MetricKind::Absolute, MetricValue::Counter { .. }) => {
                self.state.absolute_to_incremental(metric)
            }
            // Convert incremental gauges into absolute ones
            (MetricKind::Incremental, MetricValue::Gauge { .. }) => {
                self.state.incremental_to_absolute(metric)
            }
            (MetricKind::Incremental, _) => self.state.aggregate_incremental(metric),
            (MetricKind::Absolute, _) => Some(metric),
        }
    }

    fn fresh(&self, metrics: &MetricSet) -> Self {
        let state = self.state.fresh(metrics);
        Self { state }
    }
}

/// This normalization state converts all metrics into absolute metrics
/// by using the state buffer to keep track of the value throughout the
/// entire application uptime. New metrics start with a zero state.
#[derive(Default)]
pub struct AbsoluteMetricsState {
    state: MetricSet,
}

impl MetricsState for AbsoluteMetricsState {
    fn apply_state(&mut self, metric: Metric) -> Option<Metric> {
        match metric.data.kind {
            MetricKind::Absolute => Some(metric),
            MetricKind::Incremental => self.state.incremental_to_absolute(metric),
        }
    }

    fn fresh(&self, _metrics: &MetricSet) -> Self {
        // This state only stores and outputs absolute metrics, so nothing complicated to do here.
        let state = self.state.clone();
        Self { state }
    }
}

/// This normalization state converts all metrics into incremental
/// metrics by using the state buffer to keep track of the previous
/// absolute value and then generating incremental values for all
/// subsequent metrics.
#[derive(Default)]
pub struct IncrementalMetricsState {
    state: MetricSet,
}

impl MetricsState for IncrementalMetricsState {
    fn apply_state(&mut self, metric: Metric) -> Option<Metric> {
        match metric.data.kind {
            // Aggregate incremental metrics together
            MetricKind::Incremental => self.state.aggregate_incremental(metric),
            MetricKind::Absolute => self.state.absolute_to_incremental(metric),
        }
    }

    fn fresh(&self, metrics: &MetricSet) -> Self {
        let state = self.state.fresh(metrics);
        Self { state }
    }
}

/// A metric state handler specialized for the types of metrics that
/// Datadog expects. In particular, it can handle absolute gauges but
/// everything else should be incremental.
#[derive(Default)]
pub struct DatadogMetricsState {
    state: MetricSet,
}

impl MetricsState for DatadogMetricsState {
    fn apply_state(&mut self, metric: Metric) -> Option<Metric> {
        match (metric.data.kind, &metric.data.value) {
            // Datadog only accepts gauges as absolute values
            (MetricKind::Absolute, MetricValue::Gauge { .. }) => Some(metric),
            (MetricKind::Incremental, MetricValue::Gauge { .. }) => {
                self.state.incremental_to_absolute(metric)
            }
            // But all others must be incremental
            (MetricKind::Absolute, _) => self.state.absolute_to_incremental(metric),
            (MetricKind::Incremental, _) => self.state.aggregate_incremental(metric),
        }
    }

    fn fresh(&self, metrics: &MetricSet) -> Self {
        let state = self.state.fresh(metrics);
        Self { state }
    }
}

/// This is a convenience newtype wrapper for HashSet<MetricEntry> that
/// provides some extra functionality.
#[derive(Clone, Default)]
pub struct MetricSet(HashSet<MetricEntry>);

impl Deref for MetricSet {
    type Target = HashSet<MetricEntry>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for MetricSet {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl MetricSet {
    fn with_capacity(capacity: usize) -> Self {
        Self(HashSet::with_capacity(capacity))
    }

    /// Convert the incremental metric into an absolute one, using the
    /// state buffer to keep track of the value throughout the entire
    /// application uptime.
    fn incremental_to_absolute(&mut self, metric: Metric) -> Option<Metric> {
        let mut entry = MetricEntry(metric.into_absolute());
        let mut existing = self.0.take(&entry).unwrap_or_else(|| {
            // Start from zero value if the entry is not found.
            MetricEntry(entry.zero())
        });
        existing.data.value.add(&entry.data.value);
        entry.data.value = existing.data.value.clone();
        self.0.insert(existing);
        Some(entry.0)
    }

    /// Convert the absolute metric into an incremental by calculating
    /// the increment from the last saved absolute state.
    fn absolute_to_incremental(&mut self, metric: Metric) -> Option<Metric> {
        let mut entry = MetricEntry(metric);
        match self.0.get(&entry) {
            Some(reference) => {
                // From the stored reference value, emit an increment
                entry.data.value.subtract(&reference.data.value);
                Some(entry.0.into_incremental())
            }
            None => {
                // No reference so store this and emit nothing
                self.0.insert(entry);
                None
            }
        }
    }

    /// Add the saved state to this incremental metric, saving it as a
    /// starting point if no previous state is present.
    fn aggregate_incremental(&mut self, metric: Metric) -> Option<Metric> {
        let mut entry = MetricEntry(metric);
        let new_entry = match self.0.take(&entry) {
            Some(mut previous) => {
                previous.data.add(&entry.data);
                entry.data = previous.data.clone();
                previous
            }
            None => entry.clone(),
        };
        self.0.replace(new_entry);
        Some(entry.0)
    }

    /// Produce a fresh state by cloning all the absolute references and
    /// updating them with any increment previously emitted in this batch.
    fn fresh(&self, metrics: &Self) -> Self {
        Self(
            self.0
                .iter()
                .filter(|entry| entry.data.kind.is_absolute())
                .cloned()
                .map(|entry| {
                    let mut entry = MetricEntry(entry.0.into_incremental());
                    if let Some(increment) = metrics.get(&entry) {
                        entry.data.value.add(&increment.data.value);
                    }
                    MetricEntry(entry.0.into_absolute())
                })
                .collect(),
        )
    }
}

fn compress_distribution(mut samples: Vec<Sample>) -> Vec<Sample> {
    if samples.is_empty() {
        return Vec::new();
    }

    samples.sort_by(|a, b| a.value.partial_cmp(&b.value).unwrap_or(Ordering::Equal));

    let mut acc = Sample {
        value: samples[0].value,
        rate: 0,
    };
    let mut result = Vec::new();

    for sample in samples {
        if acc.value == sample.value {
            acc.rate += sample.rate;
        } else {
            result.push(acc);
            acc = sample;
        }
    }
    result.push(acc);

    result
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::event::metric::{MetricKind::*, MetricValue, StatisticKind};
    use pretty_assertions::assert_eq;
    use std::collections::BTreeMap;

    type Buffer = Vec<Vec<Metric>>;

    fn tag(name: &str) -> BTreeMap<String, String> {
        vec![(name.to_owned(), "true".to_owned())]
            .into_iter()
            .collect()
    }

    fn rebuffer<State: MetricsState>(events: Vec<Metric>) -> Buffer {
        let batch_size = BatchSettings::default().bytes(9999).events(6).size;
        let mut buffer = MetricsBuffer::<State>::new(batch_size);
        let mut result = vec![];

        for event in events {
            match buffer.push(Event::Metric(event)) {
                PushResult::Overflow(_) => panic!("overflowed too early"),
                PushResult::Ok(true) => {
                    result.push(buffer.fresh_replace().finish());
                }
                PushResult::Ok(false) => (),
            }
        }

        if !buffer.is_empty() {
            result.push(buffer.finish())
        }

        // Sort each batch to provide a predictable result ordering
        result
            .into_iter()
            .map(|mut batch| {
                batch.sort_by_key(|k| format!("{:?}", k));
                batch
            })
            .collect()
    }

    fn rebuffer_incremental_counters<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        for i in 0..4 {
            // counter-0 is repeated 5 times
            events.push(sample_counter(0, "production", Incremental, i as f64));
        }

        for i in 0..4 {
            // these counters cause a buffer flush
            events.push(sample_counter(i, "staging", Incremental, i as f64));
        }

        for i in 0..4 {
            // counter-0 increments the previous buffer, the rest are new
            events.push(sample_counter(i, "production", Incremental, i as f64));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_incremental_counters() {
        let buffer = rebuffer_incremental_counters::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_counter(0, "production", Absolute, 6.0),
                sample_counter(0, "staging", Absolute, 0.0),
                sample_counter(1, "production", Absolute, 1.0),
                sample_counter(1, "staging", Absolute, 1.0),
                sample_counter(2, "staging", Absolute, 2.0),
                sample_counter(3, "staging", Absolute, 3.0),
            ]
        );

        assert_eq!(
            buffer[1],
            [
                sample_counter(2, "production", Absolute, 2.0),
                sample_counter(3, "production", Absolute, 3.0),
            ]
        );

        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn inc_buffer_incremental_counters() {
        let buffer = rebuffer_incremental_counters::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_counter(0, "production", Incremental, 6.0),
                sample_counter(0, "staging", Incremental, 0.0),
                sample_counter(1, "production", Incremental, 1.0),
                sample_counter(1, "staging", Incremental, 1.0),
                sample_counter(2, "staging", Incremental, 2.0),
                sample_counter(3, "staging", Incremental, 3.0),
            ]
        );

        assert_eq!(
            buffer[1],
            [
                sample_counter(2, "production", Incremental, 2.0),
                sample_counter(3, "production", Incremental, 3.0),
            ]
        );

        assert_eq!(buffer.len(), 2);
    }

    #[test]
    fn std_buffer_incremental_counters() {
        let buffer = rebuffer_incremental_counters::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_counter(0, "production", Incremental, 6.0),
                sample_counter(0, "staging", Incremental, 0.0),
                sample_counter(1, "production", Incremental, 1.0),
                sample_counter(1, "staging", Incremental, 1.0),
                sample_counter(2, "staging", Incremental, 2.0),
                sample_counter(3, "staging", Incremental, 3.0),
            ]
        );

        assert_eq!(
            buffer[1],
            [
                sample_counter(2, "production", Incremental, 2.0),
                sample_counter(3, "production", Incremental, 3.0),
            ]
        );

        assert_eq!(buffer.len(), 2);
    }

    fn rebuffer_absolute_counters<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        // counter-0 and -1 only emitted once
        // counter-2 and -3 emitted twice
        // counter-4 and -5 emitted once
        for i in 0..4 {
            events.push(sample_counter(i, "production", Absolute, i as f64));
        }

        for i in 2..6 {
            events.push(sample_counter(i, "production", Absolute, i as f64 * 3.0));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_absolute_counters() {
        let buffer = rebuffer_absolute_counters::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_counter(0, "production", Absolute, 0.0),
                sample_counter(1, "production", Absolute, 1.0),
                sample_counter(2, "production", Absolute, 6.0),
                sample_counter(3, "production", Absolute, 9.0),
                sample_counter(4, "production", Absolute, 12.0),
                sample_counter(5, "production", Absolute, 15.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_absolute_counters() {
        let buffer = rebuffer_absolute_counters::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_counter(2, "production", Incremental, 4.0),
                sample_counter(3, "production", Incremental, 6.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_absolute_counters() {
        let buffer = rebuffer_absolute_counters::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_counter(2, "production", Incremental, 4.0),
                sample_counter(3, "production", Incremental, 6.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn rebuffer_incremental_gauges<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        // gauge-1 emitted once
        // gauge-2 through -4 are emitted twice
        // gauge-5 emitted once
        for i in 1..5 {
            events.push(sample_gauge(i, Incremental, i as f64));
        }

        for i in 2..6 {
            events.push(sample_gauge(i, Incremental, i as f64));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_incremental_gauges() {
        let buffer = rebuffer_incremental_gauges::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_gauge(1, Absolute, 1.0),
                sample_gauge(2, Absolute, 4.0),
                sample_gauge(3, Absolute, 6.0),
                sample_gauge(4, Absolute, 8.0),
                sample_gauge(5, Absolute, 5.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_incremental_gauges() {
        let buffer = rebuffer_incremental_gauges::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_gauge(1, Incremental, 1.0),
                sample_gauge(2, Incremental, 4.0),
                sample_gauge(3, Incremental, 6.0),
                sample_gauge(4, Incremental, 8.0),
                sample_gauge(5, Incremental, 5.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_incremental_gauges() {
        let buffer = rebuffer_incremental_gauges::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_gauge(1, Absolute, 1.0),
                sample_gauge(2, Absolute, 4.0),
                sample_gauge(3, Absolute, 6.0),
                sample_gauge(4, Absolute, 8.0),
                sample_gauge(5, Absolute, 5.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn rebuffer_absolute_gauges<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        // gauge-2 emitted once
        // gauge-3 and -4 emitted twice
        // gauge-5 emitted once
        for i in 2..5 {
            events.push(sample_gauge(i, Absolute, i as f64 * 2.0));
        }

        for i in 3..6 {
            events.push(sample_gauge(i, Absolute, i as f64 * 10.0));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_absolute_gauges() {
        let buffer = rebuffer_absolute_gauges::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_gauge(2, Absolute, 4.0),
                sample_gauge(3, Absolute, 30.0),
                sample_gauge(4, Absolute, 40.0),
                sample_gauge(5, Absolute, 50.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_absolute_gauges() {
        let buffer = rebuffer_absolute_gauges::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_gauge(3, Incremental, 24.0),
                sample_gauge(4, Incremental, 32.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_absolute_gauges() {
        let buffer = rebuffer_absolute_gauges::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_gauge(2, Absolute, 4.0),
                sample_gauge(3, Absolute, 30.0),
                sample_gauge(4, Absolute, 40.0),
                sample_gauge(5, Absolute, 50.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn rebuffer_incremental_sets<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        // set-0 emitted 8 times with 4 different values
        // set-1 emitted once with 4 values
        for i in 0..4 {
            events.push(sample_set(0, Incremental, &[i]));
        }

        for i in 0..4 {
            events.push(sample_set(0, Incremental, &[i]));
        }

        events.push(sample_set(1, Incremental, &[1, 2, 3, 4]));

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_incremental_sets() {
        let buffer = rebuffer_incremental_sets::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_set(0, Absolute, &[0, 1, 2, 3]),
                sample_set(1, Absolute, &[1, 2, 3, 4]),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_incremental_sets() {
        let buffer = rebuffer_incremental_sets::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_set(0, Incremental, &[0, 1, 2, 3]),
                sample_set(1, Incremental, &[1, 2, 3, 4]),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_incremental_sets() {
        let buffer = rebuffer_incremental_sets::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_set(0, Incremental, &[0, 1, 2, 3]),
                sample_set(1, Incremental, &[1, 2, 3, 4]),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn rebuffer_incremental_distributions<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        for _ in 2..6 {
            events.push(sample_distribution_histogram(2, Incremental, 10));
        }

        for i in 2..6 {
            events.push(sample_distribution_histogram(i, Incremental, 10));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_incremental_distributions() {
        let buffer = rebuffer_incremental_distributions::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_distribution_histogram(2, Absolute, 50),
                sample_distribution_histogram(3, Absolute, 10),
                sample_distribution_histogram(4, Absolute, 10),
                sample_distribution_histogram(5, Absolute, 10),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_incremental_distributions() {
        let buffer = rebuffer_incremental_distributions::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_distribution_histogram(2, Incremental, 50),
                sample_distribution_histogram(3, Incremental, 10),
                sample_distribution_histogram(4, Incremental, 10),
                sample_distribution_histogram(5, Incremental, 10),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_incremental_distributions() {
        let buffer = rebuffer_incremental_distributions::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_distribution_histogram(2, Incremental, 50),
                sample_distribution_histogram(3, Incremental, 10),
                sample_distribution_histogram(4, Incremental, 10),
                sample_distribution_histogram(5, Incremental, 10),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn compress_distributions() {
        let samples = crate::samples![
            2.0 => 12,
            2.0 => 12,
            3.0 => 13,
            1.0 => 11,
            2.0 => 12,
            2.0 => 12,
            3.0 => 13
        ];

        assert_eq!(
            compress_distribution(samples),
            crate::samples![1.0 => 11, 2.0 => 48, 3.0 => 26]
        );
    }

    fn rebuffer_absolute_aggregated_histograms<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        for _ in 2..5 {
            events.push(sample_aggregated_histogram(2, Absolute, 1.0, 1, 10.0));
        }

        for i in 2..5 {
            events.push(sample_aggregated_histogram(
                i,
                Absolute,
                1.0,
                i as u32,
                i as f64 * 10.0,
            ));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_absolute_aggregated_histograms() {
        let buffer = rebuffer_absolute_aggregated_histograms::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_histogram(2, Absolute, 1.0, 2, 20.0),
                sample_aggregated_histogram(3, Absolute, 1.0, 3, 30.0),
                sample_aggregated_histogram(4, Absolute, 1.0, 4, 40.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_absolute_aggregated_histograms() {
        let buffer = rebuffer_absolute_aggregated_histograms::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [sample_aggregated_histogram(2, Incremental, 1.0, 1, 10.0)]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_absolute_aggregated_histograms() {
        let buffer = rebuffer_absolute_aggregated_histograms::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_histogram(2, Absolute, 1.0, 2, 20.0),
                sample_aggregated_histogram(3, Absolute, 1.0, 3, 30.0),
                sample_aggregated_histogram(4, Absolute, 1.0, 4, 40.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn rebuffer_incremental_aggregated_histograms<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        for _ in 0..3 {
            events.push(sample_aggregated_histogram(2, Incremental, 1.0, 1, 10.0));
        }

        for i in 1..4 {
            events.push(sample_aggregated_histogram(2, Incremental, 2.0, i, 10.0));
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_incremental_aggregated_histograms() {
        let buffer = rebuffer_incremental_aggregated_histograms::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_histogram(2, Absolute, 1.0, 3, 30.0),
                sample_aggregated_histogram(2, Absolute, 2.0, 6, 30.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_incremental_aggregated_histograms() {
        let buffer = rebuffer_incremental_aggregated_histograms::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_histogram(2, Incremental, 1.0, 3, 30.0),
                sample_aggregated_histogram(2, Incremental, 2.0, 6, 30.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_incremental_aggregated_histograms() {
        let buffer = rebuffer_incremental_aggregated_histograms::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_histogram(2, Incremental, 1.0, 3, 30.0),
                sample_aggregated_histogram(2, Incremental, 2.0, 6, 30.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn rebuffer_aggregated_summaries<State: MetricsState>() -> Buffer {
        let mut events = Vec::new();
        for factor in 0..10 {
            for num in 2..5 {
                events.push(sample_aggregated_summary(
                    num,
                    Absolute,
                    (factor + num) as f64,
                ));
            }
        }

        rebuffer::<State>(events)
    }

    #[test]
    fn abs_buffer_aggregated_summaries() {
        let buffer = rebuffer_aggregated_summaries::<AbsoluteMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_summary(2, Absolute, 11.0),
                sample_aggregated_summary(3, Absolute, 12.0),
                sample_aggregated_summary(4, Absolute, 13.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn inc_buffer_aggregated_summaries() {
        let buffer = rebuffer_aggregated_summaries::<IncrementalMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_summary(2, Incremental, 9.0),
                sample_aggregated_summary(3, Incremental, 9.0),
                sample_aggregated_summary(4, Incremental, 9.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn std_buffer_aggregated_summaries() {
        let buffer = rebuffer_aggregated_summaries::<StdMetricsState>();

        assert_eq!(
            buffer[0],
            [
                sample_aggregated_summary(2, Absolute, 11.0),
                sample_aggregated_summary(3, Absolute, 12.0),
                sample_aggregated_summary(4, Absolute, 13.0),
            ]
        );

        assert_eq!(buffer.len(), 1);
    }

    fn sample_counter(num: usize, tagstr: &str, kind: MetricKind, value: f64) -> Metric {
        Metric::new(
            format!("counter-{}", num),
            kind,
            MetricValue::Counter { value },
        )
        .with_tags(Some(tag(tagstr)))
    }

    fn sample_gauge(num: usize, kind: MetricKind, value: f64) -> Metric {
        Metric::new(format!("gauge-{}", num), kind, MetricValue::Gauge { value })
    }

    fn sample_set<T: ToString>(num: usize, kind: MetricKind, values: &[T]) -> Metric {
        Metric::new(
            format!("set-{}", num),
            kind,
            MetricValue::Set {
                values: values.iter().map(|s| s.to_string()).collect(),
            },
        )
    }

    fn sample_distribution_histogram(num: u32, kind: MetricKind, rate: u32) -> Metric {
        Metric::new(
            format!("dist-{}", num),
            kind,
            MetricValue::Distribution {
                samples: crate::samples![num as f64 => rate],
                statistic: StatisticKind::Histogram,
            },
        )
    }

    fn sample_aggregated_histogram(
        num: usize,
        kind: MetricKind,
        bpower: f64,
        cfactor: u32,
        sum: f64,
    ) -> Metric {
        Metric::new(
            format!("buckets-{}", num),
            kind,
            MetricValue::AggregatedHistogram {
                buckets: crate::buckets![
                    1.0 => cfactor,
                    2.0f64.powf(bpower) => cfactor * 2,
                    4.0f64.powf(bpower) => cfactor * 4
                ],
                count: 7 * cfactor,
                sum,
            },
        )
    }

    fn sample_aggregated_summary(num: u32, kind: MetricKind, factor: f64) -> Metric {
        Metric::new(
            format!("quantiles-{}", num),
            kind,
            MetricValue::AggregatedSummary {
                quantiles: crate::quantiles![
                    0.0 => factor,
                    0.5 => factor * 2.0,
                    1.0 => factor * 4.0
                ],
                count: factor as u32 * 10,
                sum: factor * 7.0,
            },
        )
    }
}
