variable "project" {
  description = "the GCP project the run is billed to"
  type        = string
}

variable "region" {
  type    = string
  default = "asia-southeast1"
}

variable "zone" {
  type    = string
  default = "asia-southeast1-b"
}

# c3-standard-16: 16 vCPU (Sapphire Rapids), 64 GiB, no burst credits to
# measure. About $1 an hour in this region.
variable "machine_type" {
  type    = string
  default = "c3-standard-16"
}

variable "disk_gb" {
  type    = number
  default = 200
}

variable "name" {
  type    = string
  default = "boostsearch-bench"
}

variable "source_url" {
  description = "where the VM clones BoostSearch from"
  type        = string
  default     = "https://github.com/codefin-lab/boostsearch"
}

variable "source_ref" {
  description = "the commit or branch it builds"
  type        = string
  default     = "main"
}

variable "opensearch_image" {
  type    = string
  default = "opensearchproject/opensearch:3.1.0"
}

# the same heap OpenSearch's own docs give a 64 GiB machine; BoostSearch
# takes what it needs
variable "java_heap" {
  type    = string
  default = "16g"
}

variable "docs" {
  description = "how many documents the corpus has"
  type        = number
  default     = 200000
}
