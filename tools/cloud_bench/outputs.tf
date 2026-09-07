output "bucket" {
  value = google_storage_bucket.results.name
}

output "instance" {
  value = google_compute_instance.bench.name
}

output "zone" {
  value = google_compute_instance.bench.zone
}
