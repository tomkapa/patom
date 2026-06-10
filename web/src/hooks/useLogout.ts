import { useMutation, useQueryClient } from "@tanstack/react-query";
import { useNavigate } from "react-router-dom";
import { api } from "../lib/api";
import { useAuthStore } from "../stores/authStore";
import { reset as resetAnalytics } from "../lib/analytics";

export function useLogout() {
  const qc = useQueryClient();
  const navigate = useNavigate();
  const clearMe = useAuthStore((s) => s.clearMe);
  const clearError = useAuthStore((s) => s.clearError);
  return useMutation({
    mutationFn: api.logout,
    onSuccess: () => {
      qc.clear();
      clearMe();
      clearError();
      // Drop the PostHog identity so the next user on this browser starts
      // a fresh session rather than inheriting the previous one.
      resetAnalytics();
      navigate("/sign-in", { replace: true });
    },
  });
}
