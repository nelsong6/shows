# Remote state in Azure Storage (backend config passed via -backend-config in CI).
# OIDC auth for the Azure providers — no static credentials stored.
#
# `required_providers` lives in `shared-providers.tf`, which the
# tofu-plan-apply-template curls in from infra-bootstrap/main at CI time.
# Declaring required_providers locally would be a duplicate-block error.

terraform {
  backend "azurerm" {}
}

provider "azurerm" {
  features {}
  use_oidc = true
}
