import { fetchJson } from "@ironmesh/api";

export type FleetProfileCount = {
  hardware_profile_id: string;
  subject_count: number;
};

export type FleetCountryCount = {
  country_code: string;
  subject_count: number;
};

export type FleetDashboard = {
  schema_version: number;
  generated_at_unix: number;
  software_version: string;
  k_anonymity_min: number;
  total_subjects: number;
  by_hardware_profile: FleetProfileCount[];
  by_country: FleetCountryCount[];
};

/**
 * The dashboard is intentionally served by the collector itself, so this relative request stays
 * same-origin and has no credential or cross-origin requirements.
 */
export function getFleetDashboard(): Promise<FleetDashboard> {
  return fetchJson<FleetDashboard>("/v1/stats/dashboard");
}
