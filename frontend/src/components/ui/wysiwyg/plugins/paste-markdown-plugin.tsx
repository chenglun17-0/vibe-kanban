import { useEffect, useRef } from 'react';
import { useLexicalComposerContext } from '@lexical/react/LexicalComposerContext';
import {
  PASTE_COMMAND,
  COMMAND_PRIORITY_LOW,
  $getRoot,
  $getSelection,
  $isParagraphNode,
  $isRangeSelection,
  $createParagraphNode,
  $setSelection,
} from 'lexical';
import {
  $convertFromMarkdownString,
  type Transformer,
} from '@lexical/markdown';

type Props = {
  transformers: Transformer[];
};

/**
 * Plugin that handles paste with markdown conversion.
 *
 * Behavior:
 * - CMD+V: Convert the clipboard's plain-text representation to editor nodes
 * - CMD+SHIFT+V: Insert plain text as-is (raw paste)
 */
export function PasteMarkdownPlugin({ transformers }: Props) {
  const [editor] = useLexicalComposerContext();
  const shiftHeldRef = useRef(false);

  useEffect(() => {
    const rootElement = editor.getRootElement();
    if (!rootElement) return;

    // Track Shift key state during paste shortcut
    const handleKeyDown = (e: KeyboardEvent) => {
      if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === 'v') {
        shiftHeldRef.current = e.shiftKey;
      }
    };

    const handleKeyUp = () => {
      shiftHeldRef.current = false;
    };

    rootElement.addEventListener('keydown', handleKeyDown);
    rootElement.addEventListener('keyup', handleKeyUp);

    const unregisterPaste = editor.registerCommand(
      PASTE_COMMAND,
      (event) => {
        if (!(event instanceof ClipboardEvent)) return false;

        const clipboardData = event.clipboardData;
        if (!clipboardData) return false;

        // Browser selections usually include both HTML and plain text. Always
        // handle the plain-text form so rich clipboard data cannot be dropped.
        const plainText = clipboardData.getData('text/plain');
        if (!plainText) return false;

        event.preventDefault();

        editor.update(() => {
          const selection = $getSelection();
          if (!$isRangeSelection(selection)) return;

          // CMD+SHIFT+V: Raw paste - insert plain text as-is
          if (shiftHeldRef.current) {
            selection.insertRawText(plainText);
            return;
          }

          // Markdown conversion changes the active selection and creates nodes
          // under its target, so use an attached temporary container and then
          // restore the user's original caret before inserting the result.
          const originalSelection = selection.clone();
          const tempContainer = $createParagraphNode();
          $getRoot().append(tempContainer);

          try {
            $convertFromMarkdownString(plainText, transformers, tempContainer);

            const convertedNodes = tempContainer.getChildren();
            const nodes =
              convertedNodes.length === 1 && $isParagraphNode(convertedNodes[0])
                ? convertedNodes[0].getChildren()
                : convertedNodes;

            nodes.forEach((node) => node.remove());
            tempContainer.remove();
            $setSelection(originalSelection);

            const restoredSelection = $getSelection();
            if (!$isRangeSelection(restoredSelection)) return;
            if (nodes.length === 0) {
              restoredSelection.insertRawText(plainText);
              return;
            }
            restoredSelection.insertNodes(nodes);
          } catch {
            tempContainer.remove();
            $setSelection(originalSelection);
            const restoredSelection = $getSelection();
            if ($isRangeSelection(restoredSelection)) {
              restoredSelection.insertRawText(plainText);
            }
          }
        });

        return true;
      },
      COMMAND_PRIORITY_LOW
    );

    return () => {
      rootElement.removeEventListener('keydown', handleKeyDown);
      rootElement.removeEventListener('keyup', handleKeyUp);
      unregisterPaste();
    };
  }, [editor, transformers]);

  return null;
}
