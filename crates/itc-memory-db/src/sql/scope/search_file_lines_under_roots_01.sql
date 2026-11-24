SELECT p.slug AS project, l.root_path, d.path, d.freshness_status,
       CASE WHEN matched.line IS NULL THEN NULL
            ELSE COALESCE(c.start_line, 1)::bigint + matched.ordinality - 1 END AS line_number,
       matched.line
FROM source_documents d
JOIN projects p ON p.id = d.project_id
JOIN repository_locations l ON l.id = d.repository_location_id
JOIN document_revisions v ON v.source_document_id = d.id AND v.is_current
JOIN document_chunks c ON c.document_revision_id = v.id
LEFT JOIN LATERAL (
    SELECT split.line, split.ordinality
    FROM regexp_split_to_table(c.content, E'\n') WITH ORDINALITY AS split(line, ordinality)
    WHERE strpos(split.line, $1) > 0
) matched ON TRUE
WHERE ($2::text IS NULL OR p.slug = $2)
  AND ($3::text IS NULL OR d.freshness_status = $3)
  AND l.root_path = ANY($5::text[])
  AND (strpos(c.content, $1) > 0 OR strpos(lower(d.path), lower($1)) > 0)
  AND (matched.line IS NOT NULL OR strpos(lower(d.path), lower($1)) > 0)
ORDER BY (d.freshness_status = 'current') DESC, l.root_path, d.path, matched.ordinality
LIMIT $4;
