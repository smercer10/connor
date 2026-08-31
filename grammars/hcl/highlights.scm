; Hand-written for connor: the tree-sitter-hcl crate ships no query.
; Restrained to match the palette — labels, attribute names and
; interpolation markers stay plain.

(comment) @comment
(string_lit) @string
(quoted_template) @string
(heredoc_template) @string
(numeric_lit) @number
(bool_lit) @constant.builtin
(null_lit) @constant.builtin
(block (identifier) @keyword)
(function_call (identifier) @function)
