# The bench matrix on hardware a release would be cut on: one machine on
# GCP, rented for the length of the run.
#
# What this makes: a bucket for the numbers, and a VM whose startup script
# runs both engines side by side in containers, runs tools/bench_matrix.py,
# and copies the result into the bucket. The VM is not told to shut itself
# down -- `terraform destroy` gives it back, and the driver script
# (tools/cloud_bench_gcp.sh) does that on its way out whatever happened.
#
# Nothing here runs by itself: `terraform apply` costs money on the
# project's account, and the driver script asks first.

terraform {
  required_version = ">= 1.5"
  required_providers {
    google = {
      source  = "hashicorp/google"
      version = "~> 6.0"
    }
  }
}

provider "google" {
  project = var.project
  region  = var.region
  zone    = var.zone
}

# each run gets its own bucket name, so two runs never read each other's
# numbers; the bucket goes with the run
resource "random_id" "run" {
  byte_length = 3
}

resource "google_storage_bucket" "results" {
  name                        = "${var.name}-${random_id.run.hex}"
  location                    = upper(var.region)
  force_destroy               = true
  uniform_bucket_level_access = true
}

# the VM writes to the bucket as its own service account, and to nothing
# else in the project
resource "google_service_account" "bench" {
  account_id   = "${var.name}-${random_id.run.hex}"
  display_name = "boostsearch bench runner"
}

resource "google_storage_bucket_iam_member" "writer" {
  bucket = google_storage_bucket.results.name
  role   = "roles/storage.objectAdmin"
  member = "serviceAccount:${google_service_account.bench.email}"
}

resource "google_compute_instance" "bench" {
  name         = "${var.name}-${random_id.run.hex}"
  machine_type = var.machine_type
  zone         = var.zone

  boot_disk {
    initialize_params {
      image = "debian-cloud/debian-12"
      size  = var.disk_gb
      # a search engine bench on a slow disk measures the disk
      type = "pd-ssd"
    }
  }

  network_interface {
    network = "default"
    # an address so the machine can pull images and the source; nothing
    # listens on it -- both engines bind to the loopback
    access_config {}
  }

  service_account {
    email  = google_service_account.bench.email
    scopes = ["https://www.googleapis.com/auth/devstorage.read_write", "https://www.googleapis.com/auth/logging.write"]
  }

  metadata = {
    enable-oslogin = "TRUE"
  }

  metadata_startup_script = templatefile("${path.module}/startup.sh.tftpl", {
    bucket     = google_storage_bucket.results.name
    source_url = var.source_url
    source_ref = var.source_ref
    opensearch = var.opensearch_image
    java_heap  = var.java_heap
    docs       = var.docs
  })

  labels = {
    purpose = "boostsearch-bench"
  }

  depends_on = [google_storage_bucket_iam_member.writer]
}
