INSERT INTO document_chunks (id,document_revision_id,ordinal,content) VALUES ($1,$2,0,$3) ON CONFLICT (document_revision_id,ordinal) DO UPDATE SET content=EXCLUDED.content
