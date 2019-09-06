use vm::types::{Value, TypeSignature, TupleTypeSignature, parse_name_type_pairs};
use vm::callables::{DefinedFunction, DefineType};
use vm::representations::{SymbolicExpression, ClarityName};
use vm::representations::SymbolicExpressionType::{Atom, AtomValue, List};
use vm::errors::{RuntimeErrorType, CheckErrors, InterpreterResult as Result, check_argument_count};
use vm::contexts::{ContractContext, LocalContext, Environment};
use vm::eval;

define_named_enum!(DefineFunctions {
    Constant("define-constant"),
    PrivateFunction("define-private"),
    PublicFunction("define-public"),
    ReadOnlyFunction("define-read-only"),
    Map("define-map"),
    PersistedVariable("define-data-var"),
    FungibleToken("define-fungible-token"),
    NonFungibleToken("define-non-fungible-token"),
});

pub enum DefineResult {
    Variable(ClarityName, Value),
    Function(ClarityName, DefinedFunction),
    Map(String, TupleTypeSignature, TupleTypeSignature),
    PersistedVariable(String, TypeSignature, Value),
    FungibleToken(String, Option<i128>),
    NonFungibleAsset(String, TypeSignature),
    NoDefine
}

fn check_legal_define(name: &str, contract_context: &ContractContext) -> Result<()> {
    use vm::is_reserved;

    if is_reserved(name) || contract_context.variables.contains_key(name) || contract_context.functions.contains_key(name) {
        Err(CheckErrors::NameAlreadyUsed(name.to_string()).into())
    } else {
        Ok(())
    }
}

fn handle_define_variable(variable: &ClarityName, expression: &SymbolicExpression, env: &mut Environment) -> Result<DefineResult> {
    // is the variable name legal?
    check_legal_define(variable, &env.contract_context)?;
    let context = LocalContext::new();
    let value = eval(expression, env, &context)?;
    Ok(DefineResult::Variable(variable.clone(), value))
}

fn handle_define_function(signature: &[SymbolicExpression],
                          expression: &SymbolicExpression,
                          env: &Environment,
                          define_type: DefineType) -> Result<DefineResult> {
    let (function_symbol, arg_symbols) = signature.split_first()
        .ok_or(CheckErrors::DefineFunctionBadSignature)?;

    let function_name = function_symbol.match_atom()
        .ok_or(CheckErrors::ExpectedName)?;

    check_legal_define(&function_name, &env.contract_context)?;

    let arguments = parse_name_type_pairs(arg_symbols)?;

    for (argument, _) in arguments.iter() {
        check_legal_define(argument, &env.contract_context)?;
    }

    let function = DefinedFunction::new(
        arguments,
        expression.clone(),
        define_type,
        function_name,
        &env.contract_context.name);

    Ok(DefineResult::Function(function_name.clone(), function))
}

fn handle_define_persisted_variable(variable_name: &SymbolicExpression, value_type: &SymbolicExpression, value: &SymbolicExpression, env: &mut Environment) -> Result<DefineResult> {
    let variable_str = variable_name.match_atom()
        .ok_or(CheckErrors::ExpectedName)?;

    check_legal_define(&variable_str, &env.contract_context)?;

    let value_type_signature = TypeSignature::parse_type_repr(value_type)?;

    let context = LocalContext::new();
    let value = eval(value, env, &context)?;

    Ok(DefineResult::PersistedVariable(variable_str.to_string(), value_type_signature, value))
}

fn handle_define_nonfungible_asset(asset_name: &SymbolicExpression, key_type: &SymbolicExpression, env: &mut Environment) -> Result<DefineResult> {
    let asset_name = asset_name.match_atom()
        .ok_or(CheckErrors::ExpectedName)?;

    check_legal_define(&asset_name, &env.contract_context)?;

    let key_type_signature = TypeSignature::parse_type_repr(key_type)?;

    Ok(DefineResult::NonFungibleAsset(asset_name.to_string(), key_type_signature))
}

fn handle_define_fungible_token(asset_name: &SymbolicExpression, total_supply: Option<&SymbolicExpression>, env: &mut Environment) -> Result<DefineResult> {
    let asset_name = asset_name.match_atom()
        .ok_or(CheckErrors::ExpectedName)?;

    check_legal_define(&asset_name, &env.contract_context)?;

    if let Some(total_supply_expr) = total_supply {
        let context = LocalContext::new();
        let total_supply_value = eval(total_supply_expr, env, &context)?;
        if let Value::Int(total_supply_int) = total_supply_value {
            if total_supply_int <= 0 {
                Err(RuntimeErrorType::NonPositiveTokenSupply.into())
            } else {
                Ok(DefineResult::FungibleToken(asset_name.to_string(), Some(total_supply_int)))
            }
        } else {
            Err(CheckErrors::TypeValueError(TypeSignature::IntType, total_supply_value).into())
        }
    } else {
        Ok(DefineResult::FungibleToken(asset_name.to_string(), None))
    }
}

fn handle_define_map(map_name: &SymbolicExpression,
                     key_type: &SymbolicExpression,
                     value_type: &SymbolicExpression,
                     env: &Environment) -> Result<DefineResult> {
    let map_str = map_name.match_atom()
        .ok_or(CheckErrors::ExpectedName)?;

    check_legal_define(&map_str, &env.contract_context)?;

    let key_type_signature = TupleTypeSignature::parse_name_type_pair_list(key_type)?;
    let value_type_signature = TupleTypeSignature::parse_name_type_pair_list(value_type)?;

    Ok(DefineResult::Map(map_str.to_string(), key_type_signature, value_type_signature))
}

impl DefineFunctions {
    /// Try to parse a Top-Level Expression (e.g., (define-private (foo) 1)) as
    /// a define-statement, returns None if the supplied expression is not a define.
    pub fn try_parse(expression: &SymbolicExpression) -> Option<(DefineFunctions, &[SymbolicExpression])> {
        let expression = expression.match_list()?;
        let (function_name, function_args) = expression.split_first()?;
        let function_name = function_name.match_atom()?;
        let define_type = DefineFunctions::lookup_by_name(function_name)?;
        Some((define_type, function_args))
    }
}

pub fn evaluate_define(expression: &SymbolicExpression, env: &mut Environment) -> Result<DefineResult> {
    if let Some((define_type, args)) = DefineFunctions::try_parse(expression) {
        match define_type {
            DefineFunctions::Constant => {
                check_argument_count(2, args)?;
                let variable = args[0].match_atom()
                    .ok_or(CheckErrors::ExpectedName)?;
                handle_define_variable(variable, &args[1], env)
            },
            DefineFunctions::PrivateFunction => {
                check_argument_count(2, args)?;
                let function_signature = args[0].match_list()
                    .ok_or(CheckErrors::DefineFunctionBadSignature)?;
                handle_define_function(&function_signature, &args[1], env, DefineType::Private)
            },
            DefineFunctions::ReadOnlyFunction => {
                check_argument_count(2, args)?;
                let function_signature = args[0].match_list()
                    .ok_or(CheckErrors::DefineFunctionBadSignature)?;
                handle_define_function(&function_signature, &args[1], env, DefineType::ReadOnly)
            },
            DefineFunctions::NonFungibleToken => {
                check_argument_count(2, args)?;
                handle_define_nonfungible_asset(&args[0], &args[1], env)
            },
            DefineFunctions::FungibleToken => {
                if args.len() == 1 {
                    handle_define_fungible_token(&args[0], None, env)
                } else if args.len() == 2 {
                    handle_define_fungible_token(&args[0], Some(&args[1]), env)
                } else {
                    Err(CheckErrors::IncorrectArgumentCount(1, args.len()).into())
                }
            },
            DefineFunctions::PublicFunction => {
                check_argument_count(2, args)?;
                let function_signature = args[0].match_list()
                    .ok_or(CheckErrors::DefineFunctionBadSignature)?;
                handle_define_function(&function_signature, &args[1], env, DefineType::Public)
            },
            DefineFunctions::Map => {
                check_argument_count(3, args)?;
                handle_define_map(&args[0], &args[1], &args[2], env)
            },
            DefineFunctions::PersistedVariable => {
                check_argument_count(3, args)?;
                handle_define_persisted_variable(&args[0], &args[1], &args[2], env)
            }
        }
    } else {
        Ok(DefineResult::NoDefine)
    }
}
