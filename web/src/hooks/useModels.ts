import { useQuery } from "@tanstack/react-query";
import { api } from "../lib/api";

const KEY = ["models"] as const;

export function useModels() {
  return useQuery({
    queryKey: KEY,
    queryFn: api.models,
    staleTime: 5 * 60_000,
  });
}
