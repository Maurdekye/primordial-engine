#!/usr/bin/env bash
# Makes the cleaned copy of main's history for publication in a new folder outside the repository,
# with git filter-repo, and verifies it (see scripts/publish_clean.py). This repository is never
# rewritten.
# usage: PE_FILTER_PYTHON=<python with git-filter-repo> bash scripts/publish_clean.sh <new folder> [--ref <branch>] [--build]
# Needs scripts/publish_replacements.local (format: scripts/publish_replacements.example) and
# scripts/publish_terms.local, both gitignored. --build checks the copy's tip with cargo (through
# $PE_HEAVY if set). Exit 0 when the copy passes every check, 1 when a check fails, 2 on an error.
set -u
cd "$(dirname "$0")/.."
# The bash that runs $PE_HEAVY: this one (from native Windows Python, a bare `bash` can be WSL's).
export PE_BASH=${PE_BASH:-$BASH}
exec "${PE_FILTER_PYTHON:-python}" -I scripts/publish_clean.py "$@"
