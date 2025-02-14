use std::collections::HashMap;
use std::collections::HashSet;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use crate::dns::*;
use crate::dns_client::DnsClient;
use crate::dns_conf::SmartDnsConfig;
use crate::log::{debug, error};
use crate::middleware::*;

use lru::LruCache;
use tokio::{
    sync::{mpsc, Mutex, Notify},
    time::sleep,
};
use trust_dns_proto::op::Query;

pub struct DnsCacheMiddleware {
    cache: Arc<DnsLruCache>,
}

impl DnsCacheMiddleware {
    pub fn new(cfg: &SmartDnsConfig, client: Arc<DnsClient>) -> Self {
        let positive_min_ttl = Some(Duration::from_secs(cfg.rr_ttl_min.unwrap_or(cfg.rr_ttl())));
        let positive_max_ttl = Some(Duration::from_secs(cfg.rr_ttl_max.unwrap_or(cfg.rr_ttl())));

        let negative_min_ttl = None;
        let negative_max_ttl = None;

        let cache = Arc::new(DnsLruCache::new(
            cfg.cache_size(),
            positive_min_ttl,
            negative_min_ttl,
            positive_max_ttl,
            negative_max_ttl,
        ));

        if cfg.prefetch_domain {
            cache.prefetch_domain(client);
        }

        Self { cache }
    }
}

#[async_trait::async_trait]
impl Middleware<DnsContext, DnsRequest, DnsResponse, DnsError> for DnsCacheMiddleware {
    async fn handle(
        &self,
        ctx: &mut DnsContext,
        req: &DnsRequest,
        next: Next<'_, DnsContext, DnsRequest, DnsResponse, DnsError>,
    ) -> Result<DnsResponse, DnsError> {
        let query = req.query();

        let cached_val = self.cache.get(query.original(), Instant::now()).await;

        if cached_val.is_some() {
            debug!("name: {} using caching", query.name());
            ctx.lookup_source = LookupSource::Cache;
            return cached_val.unwrap();
        }

        let res = next.run(ctx, req).await;

        let res = match res {
            Ok(lookup) => {
                self.cache
                    .insert_records(
                        query.original().to_owned(),
                        lookup.records().to_owned().into_iter(),
                        Instant::now(),
                    )
                    .await;

                Ok(lookup)
            }
            Err(err) => Err(err),
        };

        res
    }
}

/// Maximum TTL as defined in https://tools.ietf.org/html/rfc2181, 2147483647
/// Setting this to a value of 1 day, in seconds
const MAX_TTL: u32 = 86400_u32;

/// An LRU eviction cache specifically for storing DNS records
struct DnsLruCache {
    cache: Arc<Mutex<LruCache<Query, DnsCacheEntry>>>,
    /// A minimum TTL value for positive responses.
    ///
    /// Positive responses with TTLs under `positive_max_ttl` will use
    /// `positive_max_ttl` instead.
    ///
    /// If this value is not set on the `TtlConfig` used to construct this
    /// `DnsLru`, it will default to 0.
    positive_min_ttl: Duration,
    /// A minimum TTL value for negative (`NXDOMAIN`) responses.
    ///
    /// `NXDOMAIN` responses with TTLs under `negative_min_ttl` will use
    /// `negative_min_ttl` instead.
    ///
    /// If this value is not set on the `TtlConfig` used to construct this
    /// `DnsLru`, it will default to 0.
    negative_min_ttl: Duration,
    /// A maximum TTL value for positive responses.
    ///
    /// Positive responses with TTLs over `positive_max_ttl` will use
    /// `positive_max_ttl` instead.
    ///
    ///  If this value is not set on the `TtlConfig` used to construct this
    /// `DnsLru`, it will default to [`MAX_TTL`] seconds.
    ///
    /// [`MAX_TTL`]: const.MAX_TTL.html
    positive_max_ttl: Duration,
    /// A maximum TTL value for negative (`NXDOMAIN`) responses.
    ///
    /// `NXDOMAIN` responses with TTLs over `negative_max_ttl` will use
    /// `negative_max_ttl` instead.
    ///
    ///  If this value is not set on the `TtlConfig` used to construct this
    /// `DnsLru`, it will default to [`MAX_TTL`] seconds.
    ///
    /// [`MAX_TTL`]: const.MAX_TTL.html
    negative_max_ttl: Duration,

    prefetch_notify: Arc<Notify>,
}

impl DnsLruCache {
    fn new(
        cache_size: usize,
        positive_min_ttl: Option<Duration>,
        negative_min_ttl: Option<Duration>,
        positive_max_ttl: Option<Duration>,
        negative_max_ttl: Option<Duration>,
    ) -> Self {
        let cache = Arc::new(Mutex::new(LruCache::new(
            NonZeroUsize::new(cache_size).unwrap(),
        )));
        let positive_min_ttl = positive_min_ttl.unwrap_or_else(|| Duration::from_secs(0));
        let negative_min_ttl = negative_min_ttl.unwrap_or_else(|| Duration::from_secs(0));
        let positive_max_ttl =
            positive_max_ttl.unwrap_or_else(|| Duration::from_secs(u64::from(MAX_TTL)));
        let negative_max_ttl =
            negative_max_ttl.unwrap_or_else(|| Duration::from_secs(u64::from(MAX_TTL)));

        Self {
            cache,
            positive_min_ttl,
            negative_min_ttl,
            positive_max_ttl,
            negative_max_ttl,
            prefetch_notify: Default::default(),
        }
    }

    async fn clear(&self) {
        self.cache.lock().await.clear();
    }

    async fn insert(
        &self,
        query: Query,
        records_and_ttl: Vec<(Record, u32)>,
        now: Instant,
    ) -> Lookup {
        let len = records_and_ttl.len();
        // collapse the values, we're going to take the Minimum TTL as the correct one
        let (records, ttl): (Vec<Record>, Duration) = records_and_ttl.into_iter().fold(
            (Vec::with_capacity(len), self.positive_max_ttl),
            |(mut records, mut min_ttl), (record, ttl)| {
                records.push(record);
                let ttl = Duration::from_secs(u64::from(ttl));
                min_ttl = min_ttl.min(ttl);
                (records, min_ttl)
            },
        );

        // If the cache was configured with a minimum TTL, and that value is higher
        // than the minimum TTL in the values, use it instead.
        let ttl = self.positive_min_ttl.max(ttl);
        let valid_until = now + ttl;

        // insert into the LRU
        let lookup = Lookup::new_with_deadline(query.clone(), Arc::from(records), valid_until);

        self.notify_prefetch_domain(ttl);

        if let Ok(mut cache) = self.cache.try_lock() {
            cache.put(
                query,
                DnsCacheEntry {
                    lookup: Ok(lookup.clone()),
                    valid_until,
                    origin_ttl: ttl,
                },
            );
        } else {
            debug!("Get dns cache lock to write failed");
        }

        lookup
    }

    /// inserts a record based on the name and type.
    ///
    /// # Arguments
    ///
    /// * `original_query` - is used for matching the records that should be returned
    /// * `records` - the records will be partitioned by type and name for storage in the cache
    /// * `now` - current time for use in associating TTLs
    ///
    /// # Return
    ///
    /// This should always return some records, but will be None if there are no records or the original_query matches none
    async fn insert_records(
        &self,
        original_query: Query,
        records: impl Iterator<Item = Record>,
        now: Instant,
    ) -> Option<Lookup> {
        // collect all records by name
        let records = records.fold(
            HashMap::<Query, Vec<(Record, u32)>>::new(),
            |mut map, record| {
                let mut query = Query::query(record.name().clone(), record.record_type());
                query.set_query_class(record.dns_class());

                let ttl = record.ttl();

                map.entry(query)
                    .or_insert_with(Vec::default)
                    .push((record, ttl));

                map
            },
        );

        // now insert by record type and name
        let mut lookup = None;
        for (query, records_and_ttl) in records {
            let is_query = original_query == query;
            let inserted = self.insert(query, records_and_ttl, now).await;

            if is_query {
                lookup = Some(inserted)
            }
        }

        lookup
    }

    /// This converts the ResolveError to set the inner negative_ttl value to be the
    ///  current expiration ttl.
    fn nx_error_with_ttl(_error: &mut DnsError, _new_ttl: Duration) {
        // if let ResolveError {
        //     kind:
        //         ResolveErrorKind::NoRecordsFound {
        //             ref mut negative_ttl,
        //             ..
        //         },
        //     ..
        // } = error
        // {
        //     *negative_ttl = Some(u32::try_from(new_ttl.as_secs()).unwrap_or(MAX_TTL));
        // }
    }

    /// Based on the query, see if there are any records available
    async fn get(&self, query: &Query, now: Instant) -> Option<Result<Lookup, DnsError>> {
        let mut out_of_date = false;
        let mut cache = match self.cache.try_lock() {
            Ok(t) => t,
            Err(err) => {
                debug!("Get dns cache lock to read failed, {:?}", err);
                return None;
            }
        };
        let lookup = cache.get_mut(query).and_then(|value| {
            if value.is_current(now) {
                out_of_date = false;
                let mut result = value.lookup.clone();

                if let Err(ref mut err) = result {
                    Self::nx_error_with_ttl(err, value.ttl(now));
                }
                Some(result)
            } else {
                out_of_date = true;
                None
            }
        });

        // in this case, we can preemptively remove out of data elements
        // this assumes time is always moving forward, this would only not be true in contrived situations where now
        //  is not current time, like tests...
        if out_of_date {
            cache.pop(query).unwrap();
        }

        lookup
    }

    fn notify_prefetch_domain(&self, duration: Duration) {
        if duration.is_zero() {
            return;
        }

        let prefetch_notify = self.prefetch_notify.clone();
        tokio::spawn(async move {
            sleep(duration).await;
            prefetch_notify.notify_one();
        });
    }

    fn prefetch_domain(&self, client: Arc<DnsClient>) {
        let (tx, mut rx) = mpsc::channel::<Vec<Query>>(100);

        {
            // prefetch domain.
            let cache = self.cache.clone();

            tokio::spawn(async move {
                let querying: Arc<Mutex<HashSet<Query>>> = Default::default();

                loop {
                    if let Some(queries) = rx.recv().await {
                        let client = client.clone();
                        let cache = cache.clone();
                        let querying = querying.clone();

                        for query in queries {
                            if !querying.lock().await.insert(query.clone()) {
                                continue;
                            }

                            let querying = querying.clone();
                            let cache = cache.clone();

                            let (client, name, typ) =
                                (client.clone(), query.name().to_owned(), query.query_type());

                            tokio::spawn(async move {
                                let now = Instant::now();
                                if let Ok(lookup) = client.lookup(name.clone(), typ, None).await {
                                    let min_ttl = lookup
                                        .records()
                                        .iter()
                                        .min_by_key(|r| r.ttl())
                                        .map(|r| Duration::from_secs(u64::from(r.ttl())));

                                    debug!(
                                        "Prefetch domain {} {}, elapsed {:?}, ttl {:?}",
                                        name,
                                        typ,
                                        now.elapsed(),
                                        min_ttl.unwrap_or_default()
                                    );

                                    if let Some(min_ttl) = min_ttl {
                                        if let Some(entry) = cache.lock().await.peek_mut(&query) {
                                            entry.valid_until = now + min_ttl;
                                            entry.origin_ttl = min_ttl;
                                            entry.lookup = Ok(lookup);
                                        }
                                    }
                                }

                                querying.lock().await.remove(&query);
                            });
                        }
                    }
                }
            });
        }

        {
            // check expired domain.
            let cache = self.cache.clone();

            let prefetch_notify = self.prefetch_notify.clone();

            const MIN_INTERVAL: Duration = Duration::from_secs(1);
            const MIN_TTL: Duration = Duration::from_secs(5);

            tokio::spawn(async move {
                let mut last_check = Instant::now();

                loop {
                    prefetch_notify.notified().await;
                    let now = Instant::now();
                    if now - last_check < MIN_INTERVAL {
                        continue;
                    }

                    last_check = now;
                    let mut most_recent = Duration::from_secs(MAX_TTL as u64);

                    let mut expired = vec![];

                    {
                        let mut cache = cache.lock().await;
                        let len = cache.len();
                        if len == 0 {
                            continue;
                        }

                        for (query, entry) in cache.iter_mut() {
                            // only prefetch query type ip addr
                            if !query.query_type().is_ip_addr() {
                                continue;
                            }
                            // Prefetch the domain that ttl greater than 10s to reduce cpu usage.
                            if entry.origin_ttl() < MIN_TTL {
                                debug!(
                                    "skiping {} {}, ttl:{:?}",
                                    query.name(),
                                    query.query_type(),
                                    entry.origin_ttl()
                                );
                                continue;
                            }
                            if entry.is_current(now) {
                                let ttl = entry.ttl(now);
                                most_recent = most_recent.min(ttl);
                                continue;
                            }

                            expired.push(query.to_owned());
                        }
                        debug!(
                            "Check prefetch domains(total: {}) elapsed {:?}",
                            len,
                            now.elapsed()
                        );
                    }

                    if !expired.is_empty() {
                        let tx = tx.clone();
                        tokio::spawn(async move {
                            if tx.send(expired).await.is_err() {
                                error!("Failed to send queries to prefetch domain!",);
                            }
                        });
                    }

                    let prefetch_notify = prefetch_notify.clone();
                    tokio::spawn(async move {
                        let dura = most_recent.max(MIN_INTERVAL);
                        debug!("Check domain prefetch after {:?} seconds", dura);
                        sleep(dura).await;
                        prefetch_notify.notify_one();
                    });
                }
            });
        }
    }
}

struct DnsCacheEntry {
    lookup: Result<Lookup, DnsError>,
    valid_until: Instant,
    origin_ttl: Duration,
}

impl DnsCacheEntry {
    /// Returns true if this set of ips is still valid
    fn is_current(&self, now: Instant) -> bool {
        now <= self.valid_until
    }

    /// Returns the ttl as a Duration of time remaining.
    fn ttl(&self, now: Instant) -> Duration {
        self.valid_until.saturating_duration_since(now)
    }

    fn origin_ttl(&self) -> Duration {
        self.origin_ttl
    }
}
