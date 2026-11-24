UPDATE indexing_runs SET configuration=configuration || $2 WHERE id=$1
