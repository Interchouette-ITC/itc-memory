DELETE FROM document_chunks
WHERE document_revision_id IN (
    SELECT v.id
    FROM document_revisions v
    JOIN source_documents d ON d.id = v.source_document_id
    JOIN repository_locations l ON l.id = d.repository_location_id
    WHERE l.root_path = $1 AND d.path = 'src/lib.rs' AND v.is_current
);
