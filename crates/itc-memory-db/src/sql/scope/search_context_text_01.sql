WITH ranked AS (
    SELECT p.slug project, l.root_path, d.path, d.document_kind, d.title, c.content,
        c.search_vector @@ websearch_to_tsquery('simple', $1) AS text_match,
        d.last_seen_at::text last_seen_at, d.last_seen_at last_seen_at_value, d.freshness_status,
        ts_rank(c.search_vector, websearch_to_tsquery('simple', $1)) score,
        row_number() OVER (
            PARTITION BY p.slug, l.root_path, d.path
            ORDER BY ts_rank(c.search_vector, websearch_to_tsquery('simple', $1)) DESC, c.ordinal
        ) document_rank
    FROM document_chunks c
    JOIN document_revisions v ON v.id = c.document_revision_id AND v.is_current
    JOIN source_documents d ON d.id = v.source_document_id
    LEFT JOIN repository_locations l ON l.id = d.repository_location_id
    JOIN projects p ON p.id = d.project_id
    WHERE p.slug = $2 AND d.is_current AND d.freshness_status = 'current'
        AND ($4::text[] IS NULL OR EXISTS (
            SELECT 1 FROM unnest($4::text[]) AS allowed(root_path) WHERE l.root_path = allowed.root_path
        ))
        AND (c.search_vector @@ websearch_to_tsquery('simple', $1) OR d.path ILIKE '%' || $1 || '%')
), candidates AS (
    SELECT * FROM ranked WHERE document_rank = 1
    ORDER BY score DESC, last_seen_at_value DESC, path LIMIT $3
)
SELECT project, root_path, path, document_kind, title,
    CASE WHEN text_match THEN
        ts_headline('simple', COALESCE(passage.content, candidates.content),
            websearch_to_tsquery('simple', $1),
            'MaxWords=100,MinWords=20,MaxFragments=2,StartSel=<<,StopSel=>>,FragmentDelimiter= ... ')
    ELSE left(candidates.content, 700) END excerpt,
    last_seen_at, freshness_status,
    CASE WHEN text_match THEN 'full-text match; ' ELSE 'path match; ' END
        || 'freshness=' || freshness_status reason,
    score
FROM candidates
LEFT JOIN LATERAL (
    SELECT part.content
    FROM regexp_split_to_table(candidates.content, E'\\n[[:space:]]*\\n')
        WITH ORDINALITY AS part(content, ordinal)
    WHERE candidates.text_match
        AND to_tsvector('simple', part.content) @@ websearch_to_tsquery('simple', $1)
    ORDER BY ts_rank(to_tsvector('simple', part.content), websearch_to_tsquery('simple', $1)) DESC,
        part.ordinal
    LIMIT 1
) passage ON true
ORDER BY score DESC, last_seen_at_value DESC, path
