#!/usr/bin/env bash
find ai-docs -type f -name '*.md' \
  ! -path '*/tickets/done/*' \
  ! -path '*/tickets/dropped/*' \
  ! -path '*/plans/*' \
  | sort
