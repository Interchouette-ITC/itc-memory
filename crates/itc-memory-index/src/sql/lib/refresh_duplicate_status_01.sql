UPDATE source_documents SET freshness_status='duplicate',duplicate_of_document_id=NULL WHERE project_id=$1 AND freshness_status='duplicate'
