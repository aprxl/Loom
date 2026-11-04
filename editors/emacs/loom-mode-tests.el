;;; loom-mode-tests.el --- Regression checks for Loom faces -*- lexical-binding: t; -*-
(require 'ert)
(require 'loom-mode)

(defmacro loom-test--with (source &rest body)
  (declare (indent 1))
  `(with-temp-buffer
     (insert ,source)
     (loom-mode)
     (font-lock-ensure)
     ,@body))

(defun loom-test--face (text &optional occurrence offset)
  (let ((case-fold-search nil))
    (save-excursion
      (goto-char (point-min))
      (dotimes (_ (or occurrence 1)) (search-forward text))
      (get-text-property (+ (- (point) (length text)) (or offset 0)) 'face))))

(ert-deftest loom-contextual-keywords ()
  (loom-test--with
   "pub once warp File {\n pub(get) data: str\n fun read(take self, take src: str, take: i32, from: str, get: u8) {}\n case Io(from IoError)\n case Named(from: IoError)\n}\nfun f[T](x: T) -> T where T: Show = x\ntype Even = i64 where _ % 2 == 0\ntest works() = assert(true)\nlet test = 1\nlet once = 2\nxs.take(3).once()\nget()\nfun where() = 0\nlet from = 3\n"
   (dolist (spec '(("once" 1) ("get" 1) ("take" 1) ("take" 2)
                   ("from" 2) ("where" 1) ("where" 2) ("test" 1)))
     (should (eq (apply #'loom-test--face spec) 'font-lock-keyword-face)))
   (dolist (spec '(("take" 3) ("take" 4) ("once" 2) ("once" 3)
                   ("from" 1) ("from" 3) ("from" 4) ("get" 2)
                   ("get" 3) ("where" 3) ("test" 2)))
     (should-not (eq (apply #'loom-test--face spec) 'font-lock-keyword-face)))))

(ert-deftest loom-contextual-lambdas-and-generics ()
  (loom-test--with "fun consume[T](take x: T) {}\nxs.map((take x: String, y) => x)\nfun take(take: i32) = take\n"
                   (should (eq (loom-test--face "take" 1) 'font-lock-keyword-face))
                   (should (eq (loom-test--face "take" 2) 'font-lock-keyword-face))
                   (should (eq (loom-test--face "take" 3) 'font-lock-function-name-face))
                   (should-not (eq (loom-test--face "take" 4) 'font-lock-keyword-face))))

(ert-deftest loom-comments-nest ()
  (loom-test--with "/// doc fun fake()\n/* outer \" /* inner */ fun hidden() */\nfun real() = 1 // end\n"
                   (should (eq (loom-test--face "doc") 'font-lock-doc-face))
                   (should (eq (loom-test--face "hidden") 'font-lock-comment-face))
                   (should (eq (loom-test--face "real") 'font-lock-function-name-face))
                   (should (nth 4 (syntax-ppss (save-excursion (goto-char 1) (search-forward "hidden") (point)))))
                   (should-not (nth 4 (syntax-ppss (save-excursion (goto-char 1) (search-forward "real") (point)))))))

(ert-deftest loom-templates-have-code ()
  (loom-test--with "print(\"hello {user.name}, {n as? u8:04X} {{literal}}\")\n"
                   (should (eq (loom-test--face "hello") 'font-lock-string-face))
                   (should (eq (loom-test--face "name") 'font-lock-variable-name-face))
                   (should (eq (loom-test--face "as?") 'loom-operator-face))
                   (should (eq (loom-test--face "u8") 'font-lock-type-face))
                   (should (eq (loom-test--face "04X") 'loom-interpolation-face))
                   (should (eq (loom-test--face "literal") 'font-lock-string-face))
                   (should-not (nth 3 (syntax-ppss (save-excursion (goto-char 1) (search-forward "user") (point)))))
                   (should (nth 3 (syntax-ppss (save-excursion (goto-char 1) (search-forward "hello") (point)))))))

(ert-deftest loom-nested-template-and-parenthesized-colon ()
  (loom-test--with "let s = \"{if true { \"inner {v}\" } else { \"no\" }} {(x is n: Num):?}\"\n"
                   (should (eq (loom-test--face "if") 'font-lock-keyword-face))
                   (should (eq (loom-test--face "true") 'font-lock-constant-face))
                   (should (eq (loom-test--face "inner") 'font-lock-string-face))
                   (should-not (eq (loom-test--face "v") 'font-lock-string-face))
                   (should (eq (loom-test--face "Num") 'font-lock-type-face))
                   (should (eq (loom-test--face ":?") 'loom-interpolation-face))))

(ert-deftest loom-raw-and-byte-literals ()
  (loom-test--with "let r = r\"C:\\path\\\"\nlet b = b\"{not_code}\\xFF\"\nfun after() {}\n"
                   (should (eq (loom-test--face "path") 'font-lock-string-face))
                   (should (eq (loom-test--face "not_code") 'font-lock-string-face))
                   (should (eq (loom-test--face "\\xFF") 'loom-interpolation-face))
                   (should (eq (loom-test--face "after") 'font-lock-function-name-face))
                   (should-not (nth 3 (syntax-ppss (save-excursion (goto-char 1) (search-forward "after") (point)))))))

(ert-deftest loom-triple-strings ()
  (loom-test--with "let s = \"\"\"\nfirst \"quoted\" {xs.map(x => x + 1)}\nlast\n\"\"\"\nfun after() {}\n"
                   (should (eq (loom-test--face "quoted") 'font-lock-string-face))
                   (should (eq (loom-test--face "map") 'font-lock-function-name-face))
                   (should (eq (loom-test--face "last") 'font-lock-string-face))
                   (should (eq (loom-test--face "after") 'font-lock-function-name-face))))

(ert-deftest loom-chars-and-escapes ()
  (loom-test--with "let c = '\\u{1F9F6}'\nlet quote = '\\''\nlet s = \"escaped \\\" text \\n\"\nfun after() {}"
                   (should (eq (loom-test--face "\\u{1F9F6}") 'loom-interpolation-face))
                   (should (eq (loom-test--face "text") 'font-lock-string-face))
                   (should (eq (loom-test--face "after") 'font-lock-function-name-face))))

(ert-deftest loom-primitive-widths-and-case ()
  (loom-test--with "let a: u1 = 0\nlet b: i128 = 0\nlet c: u129 = 0\nlet d: u08 = 0\nwarp T {}\nconst MAX = 1\nconst CONFIG_2 = 2\nwarp Thing {}\n"
                   (dolist (s '("u1" "i128" "T" "Thing"))
                     (should (eq (loom-test--face s) 'font-lock-type-face)))
                   (dolist (s '("u129" "u08"))
                     (should-not (eq (loom-test--face s) 'font-lock-type-face)))
                   (dolist (s '("MAX" "CONFIG_2"))
                     (should (eq (loom-test--face s) 'font-lock-constant-face)))))

(ert-deftest loom-number-boundaries-and-operators ()
  (loom-test--with "let x = 0xff\nlet y = 255u8 +% 1\nlet z = 1e-9\nlet r = 0..=10\nlet v = 64.kib()\nfun gen() ~> u8 <~ str {}\nx as| u8\n"
                   (dolist (s '("0xff" "255u8" "1e-9" "64"))
                     (should (eq (loom-test--face s) 'loom-number-face)))
                   (dolist (s '("+%" "..=" "~>" "<~" "as|"))
                     (should (eq (loom-test--face s) 'loom-operator-face)))
                   (should (eq (loom-test--face "kib") 'font-lock-function-name-face))))

(ert-deftest loom-twill-patterns ()
  (loom-test--with "match op {\n 0x00E0 => clear()\n 0x8xy4 => add(x, y)\n 0xff => f()\n 0b1??0_rrrr => g(r)\n}\nlet hex = 0xff\nlet 0bc_ssss = v\nif v is 0xaa { f() }\n"
                   (should (eq (loom-test--face "0x8xy4" 1 3) 'loom-capture-face))
                   (should (eq (loom-test--face "0xff" 1 2) 'loom-capture-face))
                   (should (eq (loom-test--face "0xff" 2 2) 'loom-number-face))
                   (should (eq (loom-test--face "0b1??0" 1 3) 'font-lock-builtin-face))
                   (should (eq (loom-test--face "rrrr") 'loom-capture-face))
                   (should (eq (loom-test--face "ssss") 'loom-capture-face))
                   (should (eq (loom-test--face "0xaa" 1 2) 'loom-capture-face))))

(ert-deftest loom-declarations-and-reflection ()
  (loom-test--with "#layout(bits, big)\nwarp Link[@a, @b] { from: ^Node@a }\nfun f[T](x: T) = x\ns.parse[u16]()\nv.(fld)\nSelf.fields\ncase North, South, East\n"
                   (should (eq (loom-test--face "layout") 'font-lock-preprocessor-face))
                   (should (eq (loom-test--face "@a" 1 1) 'font-lock-preprocessor-face))
                   (should (eq (loom-test--face "parse") 'font-lock-function-name-face))
                   (should (eq (loom-test--face "Self") 'font-lock-type-face))
                   (dolist (s '("North" "South" "East"))
                     (should (eq (loom-test--face s) 'font-lock-constant-face)))))

(ert-deftest loom-edit-invalidates-grammar ()
  (loom-test--with "let s = \"\"\"\nfun hidden() {}\n\"\"\"\nfun visible() {}\n"
                   (should (eq (loom-test--face "hidden") 'font-lock-string-face))
                   (goto-char (point-min)) (search-forward "\"\"\"")
                   (delete-region (- (point) 3) (point))
                   (font-lock-flush) (font-lock-ensure)
                   (should (eq (loom-test--face "hidden") 'font-lock-function-name-face))))

(ert-deftest loom-auto-mode ()
  (with-temp-buffer
    (setq buffer-file-name "/tmp/bolt.loom")
    (set-auto-mode)
    (should (eq major-mode 'loom-mode))))

(defconst loom-test--root (expand-file-name "../.." (file-name-directory load-file-name)))

(ert-deftest loom-corpus-fontifies ()
  (let* ((root loom-test--root)
         (files (append (directory-files-recursively (expand-file-name "examples" root) "\\.loom\\'")
                        (directory-files-recursively (expand-file-name "runtime/std" root) "\\.loom\\'")
                        (directory-files-recursively (expand-file-name "tests" root) "\\.loom\\'"))))
    (should files)
    (dolist (file files)
      (with-temp-buffer
        (insert-file-contents file)
        (loom-mode)
        (font-lock-ensure)
        (should (equal (buffer-substring-no-properties (point-min) (point-max))
                       (with-temp-buffer (insert-file-contents file) (buffer-string))))))))

(ert-deftest loom-context-after-string ()
  (loom-test--with "let s = \"text\"\ntest works() {}\n"
                   (should (eq (loom-test--face "test") 'font-lock-keyword-face))))
(ert-deftest loom-twill-expression-with-lambda ()
  (loom-test--with "let n = 0xff + xs.map(x => x).sum()\n"
                   (should (eq (loom-test--face "0xff" 1 2) 'loom-number-face))))
(ert-deftest loom-case-newline-and-multiple-payloads ()
  (loom-test--with "case First(from Error), Second(from Error)\nlet value = First(1), Other(2)\n"
                   (should (eq (loom-test--face "from" 2) 'font-lock-keyword-face))
                   (should (eq (loom-test--face "Other") 'font-lock-type-face))))
(ert-deftest loom-fontification-after-partial-edit ()
  (loom-test--with "let s = \"\"\"\nfun hidden() {}\n\"\"\"\nfun visible() {}\n"
                   (font-lock-mode 1)
                   (goto-char (point-min)) (search-forward "\"\"\"")
                   (delete-region (- (point) 3) (point))
                   (font-lock-ensure (point-min) (line-end-position))
                   (font-lock-ensure)
                   (should (eq (loom-test--face "hidden") 'font-lock-function-name-face))))

(ert-deftest loom-single-take-lambda ()
  (loom-test--with "xs.map(take x => x)\nxs.take(2)\n"
                   (should (eq (loom-test--face "take" 1) 'font-lock-keyword-face))
                   (should (eq (loom-test--face "take" 2) 'font-lock-function-name-face))))

(ert-deftest loom-suffixed-and-negative-hex-patterns ()
  (loom-test--with "match n {\n 0xffu8 => 1\n -0xaa => 2\n 0xff => 3\n}\n"
                   (should (eq (loom-test--face "0xffu8" 1 2) 'loom-number-face))
                   (should (eq (loom-test--face "0xaa" 1 2) 'loom-number-face))
                   (should (eq (loom-test--face "0xff" 2 2) 'loom-capture-face))))

(ert-deftest loom-match-arm-expressions-are-not-twill ()
  (loom-test--with "match n {\n 0 => 0xff + xs.map(x => x).sum(), 1 => 0xaa\n}\n"
                   (should (eq (loom-test--face "0xff" 1 2) 'loom-number-face))
                   (should (eq (loom-test--face "0xaa" 1 2) 'loom-number-face))))

(ert-deftest loom-tuple-index-boundaries ()
  (loom-test--with "p.0.1\nlet pi = 0.1\n"
                   (should-not (loom-test--face "p.0.1" 1 3))
                   (should (eq (loom-test--face "0.1" 2 1) 'loom-number-face))))

(ert-deftest loom-incomplete-single-line-templates-recover ()
  (loom-test--with "let s = \"{x   \nfun after() {}\n"
                   (should (eq (loom-test--face "after") 'font-lock-function-name-face))
                   (should-not (nth 3 (syntax-ppss (save-excursion
                                                     (goto-char 1) (search-forward "after") (point)))))))

(ert-deftest loom-fun-keyword-and-former-fn-identifier ()
  (loom-test--with "fun identity[T](take value: T) -> T where T: Show = value\nlet fn = 1\nlet funny = 2\n"
    (should (eq (loom-test--face "fun") 'font-lock-keyword-face))
    (should (eq (loom-test--face "identity") 'font-lock-function-name-face))
    (should (eq (loom-test--face "take") 'font-lock-keyword-face))
    (should (eq (loom-test--face "where") 'font-lock-keyword-face))
    (should-not (eq (loom-test--face "fn") 'font-lock-keyword-face))
    (should-not (eq (loom-test--face "funny") 'font-lock-keyword-face))))

(provide 'loom-mode-tests)
