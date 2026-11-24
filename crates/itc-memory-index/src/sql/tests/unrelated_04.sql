SELECT count(*) FROM source_documents WHERE path='docs/guide.md' AND freshness_status='stale' AND NOT is_current
