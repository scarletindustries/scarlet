ast.BlockExpression{
    body: [ast.Statement(ast.DeclarationStatement{
        identifier: ast.Identifier{
            name: 'users'
        }
        expression: ast.Expression(ast.ArrayExpression{
            elements: [ast.Expression(ast.StringLiteral{
                value: 'bob'
            }), ast.Expression(ast.StringLiteral{
                value: 'alice'
            }), ast.Expression(ast.StringLiteral{
                value: 'foo'
            })]
        })
    }), ast.Statement(ast.ForInStatement{
        body: [ast.Statement(ast.Expression(ast.FunctionCallExpression{
            identifier: ast.Identifier{
                name: 'println'
            }
            arguments: [ast.Expression(ast.Identifier{
                name: 'user'
            })]
            has_exclamation_mark: false
        }))]
        identifier: ast.Identifier{
            name: 'user'
        }
        expression: ast.Expression(ast.Identifier{
            name: 'users'
        })
    })]
}
=====================Compiler Bug=====================
| The above is the program parsed up until the error |
|   Plz report this on GitHub, with your full code   |
======================================================
[statement] Unhandled punc_open_brace at 9:8
