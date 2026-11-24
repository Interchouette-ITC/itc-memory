SELECT id,source_line FROM tasks WHERE source_document_id=$1 AND source_line IS NOT NULL
