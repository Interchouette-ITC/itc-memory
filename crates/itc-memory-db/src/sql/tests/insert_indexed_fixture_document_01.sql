INSERT INTO source_documents (id,project_id,repository_location_id,path,document_kind,title,byte_size,mtime_ns,freshness_status) VALUES ($1,$2,$3,$4,'documentation',$4,$5,2,'current')
