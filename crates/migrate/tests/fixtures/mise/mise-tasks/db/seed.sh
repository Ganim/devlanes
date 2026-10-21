#!/usr/bin/env bash
#MISE description="Seed the database"
set -euo pipefail
psql acme < db/seed.sql
