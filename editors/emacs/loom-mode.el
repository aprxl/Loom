;;; loom-mode.el --- Syntax highlighting for Loom -*- lexical-binding: t; -*-

;; Author: Loom contributors
;; Package-Requires: ((emacs "27.1"))
;; Keywords: languages

;;; Commentary:
;; A dependency-free lexical grammar for Loom.  Strings are scanned recursively
;; so template expressions are code, including nested strings and comments.
;; A second pass uses balanced token groups to distinguish contextual keywords
;; from identifiers.  This is syntax highlighting, not name/type resolution.

;;; Code:
(require 'cl-lib)

(defgroup loom nil "The Loom language." :group 'languages)
(defface loom-number-face '((t :inherit font-lock-constant-face))
  "Numbers, including the fixed digits of twill patterns." :group 'loom)
(defface loom-operator-face '((t :inherit font-lock-builtin-face))
  "Loom operators." :group 'loom)
(defface loom-interpolation-face '((t :inherit font-lock-preprocessor-face))
  "Template braces, escapes and format specifications." :group 'loom)
(defface loom-capture-face '((t :inherit font-lock-variable-name-face))
  "Capture digits in twill patterns." :group 'loom)

(defconst loom--keywords
  '("fun" "let" "var" "const" "type" "warp" "weft" "weave" "into"
    "through" "case" "pub" "if" "else" "match" "for" "in" "while"
    "loop" "break" "continue" "return" "fail" "is" "not" "and" "or"
    "mend" "defer" "ref" "spool" "given" "loom" "thread" "yield" "pull"
    "unsafe" "extern" "use" "as" "any" "mut"))
(defconst loom--operators
  (regexp-opt '("..=" "+%=" "-%=" "*%=" "+|=" "-|=" "*|=" "<<=" ">>="
                ".." "==" "!=" "<=" ">=" "<<" ">>" "=>" "->" "~>" "<~"
                "+=" "-=" "*=" "/=" "%=" "&=" "|=" "~=" "+%" "-%" "*%"
                "+|" "-|" "*|" "+?" "-?" "*?" "as%" "as|" "as?"
                "?" "^" "=" "<" ">" "+" "-" "*" "/" "%" "&" "|" "~" "!")))
(defconst loom--identifier "[[:alpha:]_][[:alnum:]_]*")
(defconst loom--decimal
  "[0-9][0-9_]*\\(?:\\.[0-9][0-9_]*\\)?\\(?:[eE][+-]?[0-9][0-9_]*\\)?\\(?:[iu]\\(?:size\\|[0-9]+\\)\\|f\\(?:16\\|32\\|64\\)\\)?")

(defvar-local loom--tick nil)
(defvar loom--tokens nil)
;; Tokens are [text start end kind parent matching-close].  These bindings
;; are private to one scan, including scans of recursively nested templates.

(defvar loom-mode-syntax-table
  (let ((table (make-syntax-table)))
    (modify-syntax-entry ?_ "w" table)
    (modify-syntax-entry ?/ ". 124b" table)
    (modify-syntax-entry ?* ". 23n" table)
    (modify-syntax-entry ?\n "> b" table)
    (modify-syntax-entry ?\" "." table)
    (modify-syntax-entry ?\' "." table)
    (modify-syntax-entry ?\\ "\\" table)
    (dolist (c '(?+ ?- ?= ?< ?> ?& ?| ?~ ?! ?? ?^ ?@ ?# ?%))
      (modify-syntax-entry c "." table))
    table))

(defun loom--paint (start end face)
  (when (< start end) (put-text-property start end 'loom--face face)))
(defun loom--fence (pos)
  (when (< pos (point-max))
    (put-text-property pos (1+ pos) 'syntax-table (string-to-syntax "|"))))
(defun loom--token (start kind)
  (push (vector (buffer-substring-no-properties start (point))
                start (point) kind nil nil) loom--tokens))

(defun loom--comment ()
  "Scan a line or nested block comment at point."
  (let ((start (point)) (doc (looking-at "///")))
    (if (looking-at "//")
        (end-of-line)
      (forward-char 2)
      (let ((depth 1))
        (while (and (> depth 0) (re-search-forward "/\\*\\|\\*/" nil t))
          (setq depth (+ depth (if (equal (match-string 0) "/*") 1 -1))))
        (when (> depth 0) (goto-char (point-max)))))
    (loom--paint start (point) (if doc 'font-lock-doc-face 'font-lock-comment-face))))

(defun loom--string (kind)
  "Scan a string or char at point; KIND is text, triple, raw, byte or char."
  (let* ((start (point))
         (prefix (if (memq kind '(raw byte)) 1 0))
         (width (if (eq kind 'triple) 3 1))
         (delimiter (if (eq kind 'char) "'" (if (eq kind 'triple) "\"\"\"" "\"")))
         (segment start) (done nil))
    (forward-char prefix)
    (loom--fence (point))
    (forward-char width)
    (while (and (not done) (< (point) (point-max)))
      (cond
       ((looking-at (regexp-quote delimiter))
        (loom--fence (point))
        (forward-char width)
        (setq done t))
       ((and (not (eq kind 'triple)) (eq (char-after) ?\n))
        ;; Recover at the newline while a single-line literal is being edited.
        (loom--fence (point))
        (setq done t))
       ((and (not (eq kind 'raw)) (eq (char-after) ?\\))
        (let ((escape (point)))
          (forward-char)
          (cond
           ((looking-at "u{[0-9A-Fa-f_]+}") (goto-char (match-end 0)))
           ((looking-at "x[0-9A-Fa-f]\\{2\\}") (goto-char (match-end 0)))
           ((< (point) (point-max)) (forward-char)))
          (loom--paint segment escape 'font-lock-string-face)
          (loom--paint escape (point) 'loom-interpolation-face)
          (setq segment (point))))
       ((and (memq kind '(text triple)) (looking-at "{{\\|}}"))
        (forward-char 2))
       ((and (memq kind '(text triple)) (eq (char-after) ?{))
        (loom--paint segment (point) 'font-lock-string-face)
        (loom--fence (point))
        (loom--paint (point) (1+ (point)) 'loom-interpolation-face)
        (let ((open (point)))
          (forward-char)
          (loom--token open 'punctuation))
        (unless (loom--code t (eq kind 'triple))
          (setq done t))
        (setq segment (point)))
       (t
        (when (and (eq kind 'raw) (eq (char-after) ?\\))
          (put-text-property (point) (1+ (point)) 'syntax-table (string-to-syntax ".")))
        (forward-char))))
    (loom--paint segment (point) 'font-lock-string-face)
    (put-text-property start (point) 'syntax-multiline t)))

(defun loom--literal (kind)
  "Scan KIND, retaining literal boundaries in the contextual token stream."
  (let ((start (point)))
    (push (vector "<literal>" start (1+ start) 'literal nil nil) loom--tokens)
    (loom--string kind)
    (push (vector "<literal>" (point) (point) 'literal nil nil) loom--tokens)))

(defun loom--code (&optional interpolation multiline)
  "Scan code, stopping at a top-level template close when INTERPOLATION.
MULTILINE permits newlines inside that interpolation."
  (let ((depth 0) (done nil))
    (while (and (not done) (< (point) (point-max)))
      (let ((start (point)))
        (cond
         ((and interpolation (= depth 0) (eq (char-after) ?}))
          (loom--fence start)
          (forward-char)
          (loom--token start 'punctuation)
          (loom--paint start (point) 'loom-interpolation-face)
          (setq done 'closed))
         ((and interpolation (= depth 0) (eq (char-after) ?:))
          (skip-chars-forward "^}\n")
          (loom--paint start (point) 'loom-interpolation-face))
         ((and interpolation (not multiline) (eq (char-after) ?\n))
          (setq done t))
         ((looking-at (if (and interpolation (not multiline))
                          "[ \t\r]+" "[ \t\r\n]+"))
          (goto-char (match-end 0)))
         ((looking-at "//\\|/\\*") (loom--comment))
         ((looking-at "r\"") (loom--literal 'raw))
         ((looking-at "b\"") (loom--literal 'byte))
         ((looking-at "\"\"\"") (loom--literal 'triple))
         ((eq (char-after) ?\") (loom--literal 'text))
         ((eq (char-after) ?\') (loom--literal 'char))
         ((looking-at "0[xob][[:alnum:]_?]+")
          (goto-char (match-end 0)) (loom--token start 'radix))
         ((looking-at (if (and loom--tokens (equal (aref (car loom--tokens) 0) "."))
                          "[0-9][0-9_]*" loom--decimal))
          (goto-char (match-end 0)) (loom--token start 'number))
         ((looking-at loom--identifier)
          (goto-char (match-end 0))
          (when (and (equal (buffer-substring-no-properties start (point)) "as")
                     (memq (char-after) '(?% ?| ??)))
            (forward-char))
          (loom--token start 'word))
         ((looking-at loom--operators)
          (goto-char (match-end 0)) (loom--token start 'operator))
         (t
          (when interpolation
            (cond ((memq (char-after) '(?\( ?\[ ?{)) (cl-incf depth))
                  ((memq (char-after) '(?\) ?\] ?})) (setq depth (max 0 (1- depth))))))
          (forward-char) (loom--token start 'punctuation)))))
    (eq done 'closed)))

(defun loom--text (tokens index)
  (when (and index (>= index 0) (< index (length tokens)))
    (aref (aref tokens index) 0)))
(defun loom--face-token (token face)
  (loom--paint (aref token 1) (aref token 2) face))
(defun loom--primitive-p (text)
  (or (member text '("bool" "char" "str" "never" "isize" "usize" "f16" "f32" "f64"))
      (and (string-match "\\`[iu]\\([1-9][0-9]*\\)\\'" text)
           (<= (string-to-number (match-string 1 text)) 128))))

(defun loom--case-name-p (tokens index)
  "Whether INDEX names a case in a possibly comma-separated declaration."
  (let ((j (1- index)) (result nil) (stop nil))
    (while (and (>= j 0) (not stop))
      (let ((text (loom--text tokens j)))
        (cond
         ((equal text "case") (setq result t stop t))
         ((member text '("=" ";" "{" "}" "fun" "let" "var" "=>")) (setq stop t))
         ((equal text ")")
          (setq j (or (aref (aref tokens j) 5) j)))
         ((and (not (equal text ","))
               (not (string-match-p "\\`[[:upper:]]" text))) (setq stop t))))
      (cl-decf j))
    result))

(defun loom--group-kind (tokens open)
  "Identify a balanced parenthesis group by its surrounding tokens."
  (let* ((before (1- open))
         (close (aref (aref tokens open) 5)))
    (when (equal (loom--text tokens before) "]")
      (setq before (1- (or (aref (aref tokens before) 5) before))))
    (cond
     ((equal (loom--text tokens (1- before)) "fun") 'params)
     ((equal (loom--text tokens (1- before)) "test") 'params)
     ((loom--case-name-p tokens before) 'case)
     ((and close (equal (loom--text tokens (1+ close)) "=>")) 'params)
     ((equal (loom--text tokens before) "pub") 'visibility))))

(defun loom--declaration-before-p (tokens index)
  "Whether INDEX lies in a declaration header, for contextual `where'."
  (let ((j (1- index)) (assignment nil) (found nil) (stop nil))
    (while (and (>= j 0) (not stop))
      (let ((text (loom--text tokens j)))
        (cond
         ((member text '("}" ")" "]"))
          (let ((open (aref (aref tokens j) 5)))
            (if open (setq j open) (setq stop t))))
         ((member text '("{" ";" "=>" "let" "var" "return")) (setq stop t))
         ((equal text "=") (setq assignment t))
         ((member text '("fun" "test" "warp" "weft" "weave" "type"))
          (setq found (or (equal text "type") (not assignment)) stop t))))
      (cl-decf j))
    found))

(defun loom--suffixed-radix-p (text)
  "Recognize a plain integer suffix, which prevents hex pattern captures."
  (and (string-match
        "\\`0\\([xob]\\)\\([0-9A-Fa-f_]+\\)\\([iu]\\(?:size\\|[1-9][0-9]*\\)\\)\\'" text)
       (let ((radix (match-string 1 text)) (digits (match-string 2 text))
             (suffix (match-string 3 text)))
         (and (loom--primitive-p suffix)
              (string-match-p (pcase radix
                                ("b" "\\`[01_]+\\'")
                                ("o" "\\`[0-7_]+\\'")
                                (_ "\\`[0-9A-Fa-f_]+\\'")) digits)))))

(defun loom--classify (tokens)
  "Assign faces to TOKENS, preserving Loom's contextual identifiers."
  ;; Link groups first; this also lets function generics and lambdas be skipped.
  (let (stack)
    (dotimes (i (length tokens))
      (let* ((token (aref tokens i)) (text (aref token 0)))
        (aset token 4 (car stack))
        (cond
         ((member text '("(" "[" "{")) (push i stack))
         ((and stack (member text '(")" "]" "}")))
          (when (equal text (cdr (assoc (loom--text tokens (car stack))
                                        '(("(" . ")") ("[" . "]") ("{" . "}")))))
            (aset token 5 (car stack))
            (aset (aref tokens (car stack)) 5 i)
            (pop stack)))))))
  (progn
    (dotimes (i (length tokens))
      (let* ((token (aref tokens i)) (text (aref token 0))
             (prev (loom--text tokens (1- i))) (next (loom--text tokens (1+ i)))
             (parent (aref token 4))
             (group (and parent (equal (loom--text tokens parent) "(")
                         (loom--group-kind tokens parent)))
             (field-start (or (and parent (= (1- i) parent)) (equal prev ",")))
             (face
              (cond
               ((memq (aref token 3) '(number radix)) 'loom-number-face)
               ((eq (aref token 3) 'operator) 'loom-operator-face)
               ((member text '("true" "false" "some" "none" "ok" "err")) 'font-lock-constant-face)
               ((equal text "Self") 'font-lock-type-face)
               ((equal text "self") 'font-lock-builtin-face)
               ((member text loom--keywords) 'font-lock-keyword-face)
               ((member text '("as%" "as|" "as?")) 'loom-operator-face)
               ((loom--primitive-p text) 'font-lock-type-face)
               ((equal prev "#") 'font-lock-preprocessor-face)
               ((equal prev "@") 'font-lock-preprocessor-face)
               ((and (equal text "once") (equal next "warp") (not (equal prev ".")))
                'font-lock-keyword-face)
               ((and (equal text "get") (eq group 'visibility) (equal next ")"))
                'font-lock-keyword-face)
               ((and (equal text "take")
                     (or (and (eq group 'params) field-start)
                         (and (not (equal prev "."))
                              (equal (loom--text tokens (+ i 2)) "=>")))
                     (or (equal next "self")
                         (and next (string-match-p "\\`[[:lower:]_]" next)))
                     (not (member next '("mut" "const")))) 'font-lock-keyword-face)
               ((and (equal text "from") (eq group 'case) field-start
                     (not (member next '(":" "," ")")))) 'font-lock-keyword-face)
               ((and (equal text "test") next
                     (string-match-p "\\`[[:lower:]_][[:alnum:]_]*\\'" next)
                     (member (loom--text tokens (+ i 2)) '("(" "["))
                     (or (= i 0) (member prev '("{" "}" ";" "pub"))
                         (and (not (member prev '("." "=" "let" "var")))
                              (string-match-p "\n" (buffer-substring-no-properties
                                                    (aref (aref tokens (1- i)) 2)
                                                    (aref token 1)))))) 'font-lock-keyword-face)
               ((and (equal text "where") (not (member prev '("." "fun" "test")))
                     (not (equal next ":")) (> i 0)
                     (not (string-match-p "\n" (buffer-substring-no-properties
                                                (aref (aref tokens (1- i)) 2) (aref token 1))))
                     (loom--declaration-before-p tokens i)) 'font-lock-keyword-face)
               ((and (eq (aref token 3) 'word)
                     (or (equal prev "fun")
                         (and (equal prev "test")
                              (eq (get-text-property (aref (aref tokens (1- i)) 1) 'loom--face)
                                  'font-lock-keyword-face)))) 'font-lock-function-name-face)
               ((and (string-match-p "\\`[[:upper:]]" text)
                     (or (equal prev "case") (equal prev ".")
                         (and (equal prev ",") (loom--case-name-p tokens i))))
                'font-lock-constant-face)
               ((and (>= (length text) 2)
                     (string-match-p "\\`[[:upper:]][[:upper:]0-9_]*\\'" text)) 'font-lock-constant-face)
               ((string-match-p "\\`[[:upper:]]" text) 'font-lock-type-face)
               ((equal text "_") 'font-lock-builtin-face)
               ((and (eq (aref token 3) 'word)
                     (or (equal next "(")
                         (and (equal next "[")
                              (let ((close (aref (aref tokens (1+ i)) 5)))
                                (and close (equal (loom--text tokens (1+ close)) "("))))))
                'font-lock-function-name-face)
               ((and (eq (aref token 3) 'word)
                     (or (equal prev ".") (equal next ":")
                         (member prev '("let" "var" "spool" "given")))) 'font-lock-variable-name-face))))
        (when face (loom--face-token token face))
        ;; The lexer treats lower-case a-f as digits in expressions, but as
        ;; captures in patterns.  Only add capture faces in a pattern context.
        (when (and (eq (aref token 3) 'radix)
                   (not (loom--suffixed-radix-p text))
                   (not (equal prev "-"))
                   (loom--pattern-p tokens i))
          (save-excursion
            (goto-char (+ (aref token 1) 2))
            (while (re-search-forward "[a-z?]+" (aref token 2) t)
              (loom--paint (match-beginning 0) (match-end 0)
                           (if (string-match-p "\\`?+\\'" (match-string 0))
                               'font-lock-builtin-face 'loom-capture-face)))))))))

(defun loom--match-parent-p (tokens index)
  "Whether INDEX is nested in a match body."
  (let ((parent (aref (aref tokens index) 4)) (found nil))
    (while (and parent (not found))
      (when (equal (loom--text tokens parent) "{")
        (let ((j (1- parent)))
          (while (and (>= j 0)
                      (not (member (loom--text tokens j) '("match" "=" "=>" ";" "{" "}"))))
            (if (member (loom--text tokens j) '("]" ")"))
                (setq j (1- (or (aref (aref tokens j) 5) j)))
              (cl-decf j)))
          (setq found (equal (loom--text tokens j) "match"))))
      (setq parent (aref (aref tokens parent) 4)))
    found))

(defun loom--pattern-p (tokens index)
  "Conservatively recognize binding, `is', loop and match-arm patterns."
  (let ((j (1- index)) (answer nil) (rhs nil) (stop nil))
    (while (and (>= j 0) (not stop))
      (let ((text (loom--text tokens j)))
        (cond
         ((member text '("let" "var" "is" "for")) (setq answer t stop t))
         ((equal text "=>")
          (setq rhs (not (string-match-p "\n" (buffer-substring-no-properties
                                               (aref (aref tokens j) 2)
                                               (aref (aref tokens index) 1))))
                stop t))
         ((member text '("=" "in" "if" "and" "or" ";" "}")) (setq stop t))
         ((equal text "{")
          (let ((k (1- j)))
            (while (and (>= k 0) (not (member (loom--text tokens k) '("match" "{" "}" ";" "=" "=>"))))
              (cl-decf k))
            (setq answer (equal (loom--text tokens k) "match") stop t)))))
      (cl-decf j))
    ;; Subsequent match arms begin after an earlier arm's expression.  A forward
    ;; arrow before the next newline identifies the pattern side of that arm.
    (or answer
        (and (not rhs) (loom--match-parent-p tokens index)
             (let ((k (1+ index)))
               (while (and (< k (length tokens))
                           (not (member (loom--text tokens k) '("=>" "=" ";" "}"))))
                 (if (member (loom--text tokens k) '("(" "[" "{"))
                     (setq k (1+ (or (aref (aref tokens k) 5) k)))
                   (cl-incf k)))
               (and (equal (loom--text tokens k) "=>")
                    (not (string-match-p "\n" (buffer-substring-no-properties
                                               (aref (aref tokens index) 2)
                                               (aref (aref tokens k) 1))))))))))

(defun loom--syntax-propertize (_start _end)
  "Refresh the lexical grammar after edits, including multiline constructs."
  (save-restriction
    (widen)
    (with-silent-modifications
      (remove-text-properties (point-min) (point-max)
                              '(loom--face nil syntax-table nil syntax-multiline nil))
      (let ((loom--tokens nil) (case-fold-search nil))
        (save-excursion
          (goto-char (point-min))
          (loom--code)
          (loom--classify (vconcat (nreverse loom--tokens)))))
      (setq loom--tick (buffer-chars-modified-tick)))))

(defun loom--extend-syntax-region (start end)
  "Extend syntax parsing to include the full lexical context."
  (unless (and (= start (point-min)) (= end (point-max)))
    (cons (point-min) (point-max))))

(defun loom--after-change (_start _end _old-length)
  "Invalidate distant faces when delimiters change their lexical context."
  (setq loom--tick nil)
  (when font-lock-mode (font-lock-flush (point-min) (point-max))))

(defun loom--font-lock (limit)
  "Find the next lexically classified span before LIMIT."
  (unless (equal loom--tick (buffer-chars-modified-tick))
    (syntax-propertize (point-max)))
  (let (start face)
    (while (and (< (point) limit) (not face))
      (setq face (get-text-property (point) 'loom--face))
      (if face (setq start (point))
        (goto-char (next-single-property-change (point) 'loom--face nil limit))))
    (when face
      (goto-char (next-single-property-change (point) 'loom--face nil limit))
      (set-match-data (list start (point)))
      t)))

;;;###autoload
(define-derived-mode loom-mode prog-mode "Loom"
  "Major mode for Loom syntax highlighting.
Handles contextual keywords, nested comments and template expressions.
Faces describe syntax; distinguishing resolved associated types from cases
requires semantic information and is outside this mode's scope."
  :syntax-table loom-mode-syntax-table
  (setq-local syntax-propertize-function #'loom--syntax-propertize)
  (add-hook 'syntax-propertize-extend-region-functions #'loom--extend-syntax-region nil t)
  (add-hook 'after-change-functions #'loom--after-change nil t)
  (setq-local font-lock-defaults
              '(((loom--font-lock (0 (get-text-property (match-beginning 0) 'loom--face) t))) t))
  (setq-local comment-start "// ")
  (setq-local comment-end "")
  (setq-local comment-start-skip "//+\\s-*\\|/\\*+\\s-*")
  (setq-local parse-sexp-lookup-properties t)
  (setq-local indent-tabs-mode nil)
  (setq-local tab-width 4)
  (setq-local loom--tick nil))

;;;###autoload
(add-to-list 'auto-mode-alist '("\\.loom\\'" . loom-mode))
(provide 'loom-mode)
;;; loom-mode.el ends here
