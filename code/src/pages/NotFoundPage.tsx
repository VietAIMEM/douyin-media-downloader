import { useNavigate } from "react-router-dom";

import { Button, EmptyState, PageHeader } from "@/components/ui";

export function NotFoundPage() {
  const navigate = useNavigate();

  return (
    <>
      <PageHeader title="Not found" />
      <EmptyState
        title="There is no screen at this address"
        description="The link you followed does not match any screen in this build."
        action={
          <Button variant="primary" onClick={() => navigate("/")}>
            Go to Home
          </Button>
        }
      />
    </>
  );
}
