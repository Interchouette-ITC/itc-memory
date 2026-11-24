# Docker Compose targets for the local PostgreSQL service.

COMPOSE_FILE := $(ROOT)/docker/compose.yml
COMPOSE := docker compose --env-file $(ROOT)/.env -f $(COMPOSE_FILE) --project-directory $(ROOT)

.PHONY: db-init db-up db-down db-status db-logs db-shell db-backup db-restore db-reset

db-init:
	@mkdir -p $(ROOT)/postgres/data $(ROOT)/postgres/backups
	@test -f $(ROOT)/.env || cp $(ROOT)/.env.example $(ROOT)/.env
	@echo "Database environment ready at $(ROOT)/.env"

db-up: db-init
	$(COMPOSE) up -d postgres

db-down:
	$(COMPOSE) stop postgres

db-status:
	$(COMPOSE) ps postgres

db-logs:
	$(COMPOSE) logs -f postgres

db-shell:
	$(COMPOSE) exec postgres sh -c 'psql -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"'

db-backup:
	@mkdir -p $(ROOT)/postgres/backups
	$(COMPOSE) exec -T postgres sh -c 'pg_dump -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"' | gzip > $(ROOT)/postgres/backups/itc-memory-$$(date -u +%Y%m%dT%H%M%SZ).sql.gz

db-restore:
	@test -n "$(FILE)" || (echo "Usage: make db-restore FILE=postgres/backups/<file>.sql.gz"; exit 2)
	@test -f "$(FILE)" || (echo "Backup file not found: $(FILE)"; exit 2)
	gunzip -c "$(FILE)" | $(COMPOSE) exec -T postgres sh -c 'psql -v ON_ERROR_STOP=1 -U "$$POSTGRES_USER" -d "$$POSTGRES_DB"'

db-reset:
	@echo "Refusing automatic data deletion. Remove the specific postgres/data directory only after making a backup."
	@exit 2
