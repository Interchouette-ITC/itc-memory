UPDATE document_revisions AS revision
SET source_commit_sha = $3
FROM source_documents AS document
WHERE document.repository_location_id = $1
  AND document.path = $2
  AND revision.source_document_id = document.id
  AND revision.is_current
  AND revision.source_commit_sha IS DISTINCT FROM $3
