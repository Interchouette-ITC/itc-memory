UPDATE plans SET is_current=false,status=CASE WHEN status='active' THEN 'archived' ELSE status END,updated_at=now() WHERE project_id=$1 AND is_current AND source_document_id IS DISTINCT FROM $2
