SELECT revision.source_commit_sha, document.freshness_status
FROM source_documents AS document
JOIN repository_locations AS location ON location.id = document.repository_location_id
JOIN document_revisions AS revision ON revision.source_document_id = document.id AND revision.is_current
WHERE location.root_path = $1 AND document.path = $2
