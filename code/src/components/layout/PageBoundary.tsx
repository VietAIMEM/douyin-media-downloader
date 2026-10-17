import { Component, type ErrorInfo, type ReactNode } from "react";

import { Button, Card } from "@/components/ui";

/**
 * Keeps one screen's failure from taking the application with it.
 *
 * React unmounts the whole tree when a render throws and nothing catches it, so
 * a single bad value in one page leaves a blank window with no way back — no
 * navigation, no message, nothing to click. That is the worst possible failure
 * for a desktop application, because there is no address bar to escape to.
 *
 * The boundary is placed around each screen rather than around the application,
 * so the sidebar and the failing screen's neighbours keep working and the user
 * can simply go somewhere else.
 *
 * A class, because this is the one thing React still requires a class for:
 * `getDerivedStateFromError` and `componentDidCatch` have no hook equivalent.
 */
interface Props {
  /** What was being shown, so the message can name it. */
  title: string;
  children: ReactNode;
}

interface State {
  error: Error | null;
}

export class PageBoundary extends Component<Props, State> {
  override state: State = { error: null };

  static getDerivedStateFromError(error: Error): State {
    return { error };
  }

  override componentDidCatch(error: Error, info: ErrorInfo) {
    // Goes to the webview console, which is where a developer looks and where
    // nothing else is written: this application keeps no error reporting.
    console.error(`the ${this.props.title} screen failed to render`, error, info.componentStack);
  }

  private retry = () => this.setState({ error: null });

  override render() {
    const { error } = this.state;
    if (!error) return this.props.children;

    return (
      <Card
        title={`${this.props.title} could not be shown`}
        description="Something in this screen failed. The rest of the application is unaffected."
      >
        <div className="space-y-3">
          <p className="text-sm text-content-secondary">
            This is a bug in the application rather than something you did. The other screens
            still work, and nothing has been lost — your downloads, history and settings are
            untouched.
          </p>
          {/* The message only. A stack trace is developer detail, and this text
              is shown to whoever is using the application. */}
          <p className="rounded-lg border border-surface-border bg-surface-raised px-3 py-2 font-mono text-xs text-content-muted">
            {error.message || error.name}
          </p>
          <Button size="sm" onClick={this.retry}>
            Try again
          </Button>
        </div>
      </Card>
    );
  }
}
