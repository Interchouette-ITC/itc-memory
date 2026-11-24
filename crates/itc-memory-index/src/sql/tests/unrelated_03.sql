SELECT COALESCE(string_agg(path || ':' || freshness_status || ':' || is_current::text, ', ' ORDER BY path), '') FROM source_documents
