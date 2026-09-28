import { useEffect } from "react";

interface UseSearchFetchTriggerOptions {
  debouncedSearch: string;
  searchRegex?: boolean;
  isComposing: boolean;
  typeFilter?: string | null;
  fetchHistory: (reset?: boolean) => void;
}

export const useSearchFetchTrigger = ({
  debouncedSearch,
  searchRegex,
  isComposing,
  typeFilter,
  fetchHistory
}: UseSearchFetchTriggerOptions) => {
  useEffect(() => {
    if (!isComposing) {
      fetchHistory(true);
    }
  }, [debouncedSearch, searchRegex, isComposing, fetchHistory]);

  useEffect(() => {
    fetchHistory(true);
  }, [typeFilter, fetchHistory]);
};
