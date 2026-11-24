UPDATE document_revisions SET is_current=false WHERE source_document_id=(SELECT id FROM source_documents WHERE path='plans/current.md')
