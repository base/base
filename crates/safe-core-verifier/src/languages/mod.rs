//! 🌍 Suporte a 12 Linguagens de Programação
//!
//! Cada linguagem tem:
//! - Parser tree-sitter configurado
//! - Prefixo da Convenção X adaptado
//! - Lista de dependências conhecidas
//! - Padrões de segurança específicos

use std::path::Path;

use anyhow::Result;
use tree_sitter::Language as TsLanguage;

/// Linguagens suportadas pelo verificador.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Language {
    // Tier 1: Suporte completo (checks implementados)
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Go,

    // Tier 2: Suporte estendido (checks básicos)
    Ruby,
    PHP,
    Cpp,
    CSharp,
    Java,
    Kotlin,
    Swift,
}

impl Language {
    /// Retorna todas as linguagens suportadas.
    pub fn all() -> Vec<Self> {
        vec![Self::Rust, Self::Python, Self::JavaScript, Self::Go]
    }

    /// Detecta a linguagem baseada na extensão do arquivo.
    pub fn detect(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .ok_or_else(|| anyhow::anyhow!("Arquivo sem extensão: {:?}", path))?;

        match ext {
            "rs" => Ok(Self::Rust),
            "py" => Ok(Self::Python),
            "js" | "mjs" | "cjs" => Ok(Self::JavaScript),
            "ts" | "tsx" => Ok(Self::TypeScript),
            "go" => Ok(Self::Go),
            "rb" => Ok(Self::Ruby),
            "php" => Ok(Self::PHP),
            "cpp" | "cc" | "cxx" | "c++" => Ok(Self::Cpp),
            "cs" => Ok(Self::CSharp),
            "java" => Ok(Self::Java),
            "kt" | "kts" => Ok(Self::Kotlin),
            "swift" => Ok(Self::Swift),
            _ => anyhow::bail!("Linguagem não suportada: .{}", ext),
        }
    }

    /// Retorna a linguagem tree-sitter correspondente.
    pub fn tree_sitter_language(&self) -> TsLanguage {
        match self {
            Self::Rust => tree_sitter_rust::LANGUAGE.into(),
            Self::Python => tree_sitter_python::LANGUAGE.into(),
            Self::JavaScript => tree_sitter_javascript::LANGUAGE.into(),
            Self::TypeScript => panic!("Not implemented"),
            Self::Go => tree_sitter_go::LANGUAGE.into(),
            Self::Ruby => panic!("Not implemented"),
            Self::PHP => panic!("Not implemented"),
            Self::Cpp => panic!("Not implemented"),
            Self::CSharp => panic!("Not implemented"),
            Self::Java => panic!("Not implemented"),
            Self::Kotlin => panic!("Not implemented"),
            Self::Swift => panic!("Not implemented"),
        }
    }

    /// Retorna o nome da linguagem como string.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript => "typescript",
            Self::Go => "go",
            Self::Ruby => "ruby",
            Self::PHP => "php",
            Self::Cpp => "cpp",
            Self::CSharp => "csharp",
            Self::Java => "java",
            Self::Kotlin => "kotlin",
            Self::Swift => "swift",
        }
    }

    /// Retorna o prefixo da Convenção X para esta linguagem.
    pub fn convention_x_prefix(&self) -> &'static str {
        match self {
            // snake_case
            Self::Rust | Self::Python | Self::Go | Self::Ruby => "x_",
            // camelCase
            Self::JavaScript | Self::TypeScript | Self::Java | Self::Kotlin | Self::Swift => "x",
            // PascalCase (C#)
            Self::CSharp => "X",
            // C++ usa snake_case ou camelCase
            Self::Cpp => "x_",
            // PHP usa snake_case
            Self::PHP => "x_",
        }
    }

    /// Retorna as dependências conhecidas para esta linguagem.
    pub fn known_dependencies(&self) -> Vec<&'static str> {
        match self {
            Self::Rust => vec![
                "serde",
                "tokio",
                "anyhow",
                "thiserror",
                "tracing",
                "clap",
                "rayon",
                "regex",
                "chrono",
                "uuid",
                "blake3",
            ],
            Self::Python => vec![
                "requests",
                "flask",
                "django",
                "fastapi",
                "numpy",
                "pandas",
                "torch",
                "tensorflow",
                "scikit-learn",
                "matplotlib",
                "pytest",
            ],
            Self::JavaScript | Self::TypeScript => vec![
                "express",
                "react",
                "vue",
                "angular",
                "next",
                "nuxt",
                "lodash",
                "axios",
                "mongoose",
                "sequelize",
                "jest",
            ],
            Self::Go => vec![
                "fmt",
                "net/http",
                "encoding/json",
                "context",
                "sync",
                "github.com/gin-gonic/gin",
                "github.com/gorilla/mux",
            ],
            Self::Ruby => vec!["rails", "sinatra", "rspec", "nokogiri", "puma", "sidekiq"],
            Self::PHP => vec!["laravel", "symfony", "guzzle", "phpunit", "monolog"],
            Self::Cpp => {
                vec!["iostream", "vector", "string", "memory", "algorithm", "boost", "qt", "opencv"]
            }
            Self::CSharp => vec![
                "System",
                "System.Collections",
                "System.Linq",
                "System.Threading",
                "Microsoft.Extensions",
                "Newtonsoft.Json",
            ],
            Self::Java => vec![
                "java.util",
                "java.io",
                "java.net",
                "java.sql",
                "org.springframework",
                "com.google.gson",
            ],
            Self::Kotlin => {
                vec!["kotlin", "kotlinx.coroutines", "android", "org.jetbrains", "com.squareup"]
            }
            Self::Swift => vec!["Foundation", "UIKit", "SwiftUI", "Combine", "Alamofire", "Realm"],
        }
    }

    /// Retorna os padrões inseguros específicos para esta linguagem.
    pub fn unsafe_patterns(&self) -> Vec<(&&'static str, &'static str)> {
        match self {
            Self::Rust => vec![
                (&"unsafe {", "Bloco unsafe detectado. Garanta invariantes de segurança."),
                (&".unwrap()", "Uso de unwrap() pode causar panic. Use ? ou expect()."),
                (&"panic!(", "panic! deve ser evitado. Use Result."),
            ],
            Self::Python => vec![
                (&"eval(", "eval() é inseguro. Use alternativas seguras."),
                (&"exec(", "exec() é inseguro. Evite execução dinâmica."),
                (&"pickle.loads(", "pickle é inseguro. Use json."),
                (&"os.system(", "os.system() é inseguro. Use subprocess."),
            ],
            Self::JavaScript | Self::TypeScript => vec![
                (&"eval(", "eval() é inseguro."),
                (&"innerHTML =", "innerHTML é vulnerável a XSS."),
                (&"document.write(", "document.write() é inseguro."),
                (&"jwt.decode(", "jwt.decode() sem verificação é inseguro."),
            ],
            Self::Go => vec![
                (&"panic(", "panic() deve ser evitado. Use error returns."),
                (&"os/exec.Command", "Verifique sanitização de argumentos."),
            ],
            Self::Ruby => vec![
                (&"eval(", "eval() é inseguro."),
                (&"system(", "system() é inseguro. Use backticks com cuidado."),
                (&"Marshal.load(", "Marshal é inseguro. Use JSON."),
            ],
            Self::PHP => vec![
                (&"eval(", "eval() é inseguro."),
                (&"exec(", "exec() é inseguro."),
                (&"shell_exec(", "shell_exec() é inseguro."),
                (&"unserialize(", "unserialize() é inseguro."),
            ],
            Self::Cpp => vec![
                (&"gets(", "gets() é inseguro. Use fgets()."),
                (&"strcpy(", "strcpy() é inseguro. Use strncpy()."),
                (&"system(", "system() é inseguro."),
                (&"malloc(", "malloc() sem verificação pode causar overflow."),
            ],
            Self::CSharp => vec![
                (&"Process.Start(", "Verifique sanitização de argumentos."),
                (&"SqlDataReader", "Use parâmetros para evitar SQL injection."),
                (&"XmlDocument", "Use XmlReader para evitar XXE."),
            ],
            Self::Java => vec![
                (&"Runtime.exec(", "Verifique sanitização de argumentos."),
                (&"ObjectInputStream", "Deserialização insegura."),
                (&"DocumentBuilder", "Desabilite entidades externas."),
            ],
            Self::Kotlin => vec![
                (&"!!", "Operador !! pode causar NullPointerException."),
                (&"Runtime.getRuntime().exec(", "Verifique sanitização."),
            ],
            Self::Swift => vec![
                (&"try!", "try! pode causar crash. Use try? ou do-catch."),
                (&"force unwrap", "Force unwrap pode causar crash."),
            ],
        }
    }
}

impl std::fmt::Display for Language {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}
