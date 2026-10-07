import { Dialog } from "./Common";
import { useI18n } from "../i18n";

/**
 * A question the user answers before something irreversible happens.
 *
 * The answer is a callback rather than a promise: whatever runs after "yes" has
 * to read the state as it is THEN, not as it was when the question went up, so
 * the caller re-enters its own action with the answer instead of resuming a
 * closure that captured the old state.
 */
export interface ConfirmationRequest {
  /** The question itself, as the window's title. */
  question: string;
  /** What the answer will do that the question does not already say. */
  detail?: string;
  confirmLabel: string;
  /** Styles the confirming button as destructive. */
  destructive?: boolean;
  onConfirm: () => void;
}

/**
 * The in-app replacement for `window.confirm`. The native one is not a
 * question here at all: the dialog plugin turns it into a function returning a
 * promise, which is always truthy, so `!window.confirm(...)` never stops
 * anything.
 */
export function ConfirmDialog({ request, onClose }: { request: ConfirmationRequest; onClose: () => void }) {
  const { t } = useI18n();
  return (
    <Dialog
      title={request.question}
      width="420px"
      onClose={onClose}
      footer={
        <>
          <button type="button" className="button" onClick={onClose}>
            {t("取消", "Cancel")}
          </button>
          <button
            type="button"
            className={`button ${request.destructive ? "button--danger" : "button--primary"}`}
            // biome-ignore lint/a11y/noAutofocus: the user has just asked for this; the answer is one key away.
            autoFocus
            onClick={() => {
              onClose();
              request.onConfirm();
            }}
          >
            {request.confirmLabel}
          </button>
        </>
      }
    >
      {request.detail && <p className="confirm-copy">{request.detail}</p>}
    </Dialog>
  );
}
