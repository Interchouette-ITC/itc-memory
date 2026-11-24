INSERT INTO document_revisions (id,source_document_id,content_hash,byte_size,is_current) SELECT $1,id,'changed-workflow-source',48,true FROM source_documents WHERE path='plans/current.md'
