-- Content-free native request measurements, independent from billing counters.
ALTER TABLE usage_requests ADD COLUMN request_metrics TEXT NOT NULL DEFAULT '{}';
