use tree_sitter::Tree;

use crate::languages::Language;

pub struct CodeStructureAnalyzer;

impl CodeStructureAnalyzer {
    pub fn analyze(tree: &Tree, code: &str, lang: Language) -> StructureMetrics {
        let root = tree.root_node();
        let mut walker = root.walk();

        let mut metrics = StructureMetrics::default();
        let mut fn_lengths: Vec<usize> = Vec::new();

        loop {
            let node = walker.node();
            let kind = node.kind();

            match lang {
                Language::Rust if kind == "function_item" => {
                    fn_lengths.push(node.end_byte() - node.start_byte());
                    Self::check_convention_x_in_node(node, code, &mut metrics);
                }
                Language::Python if kind == "function_definition" => {
                    fn_lengths.push(node.end_byte() - node.start_byte());
                    Self::check_convention_x_in_node(node, code, &mut metrics);
                }
                Language::JavaScript | Language::TypeScript if kind == "function_declaration" => {
                    fn_lengths.push(node.end_byte() - node.start_byte());
                }
                Language::Go if kind == "function_declaration" => {
                    fn_lengths.push(node.end_byte() - node.start_byte());
                }
                _ => {}
            }

            let depth = walker.depth();
            if metrics.max_depth < depth as usize {
                metrics.max_depth = depth as usize;
            }
            metrics.total_nodes += 1;

            if !walker.goto_next_sibling() && !walker.goto_parent() {
                break;
            }
        }

        if !fn_lengths.is_empty() {
            metrics.avg_function_bytes =
                fn_lengths.iter().sum::<usize>() as f64 / fn_lengths.len() as f64;
            metrics.max_function_bytes = *fn_lengths.iter().max().unwrap_or(&0);
            metrics.functions_over_200_lines = fn_lengths.iter().filter(|&&len| len > 8000).count();
        }

        metrics
    }

    fn check_convention_x_in_node(
        node: tree_sitter::Node,
        code: &str,
        metrics: &mut StructureMetrics,
    ) {
        let name = node.child_by_field_name("name");
        if let Some(name_node) = name {
            let name_str = &code[name_node.byte_range()];
            if name_str.starts_with("x_") {
                metrics.x_prefixed_functions += 1;
            }
            metrics.total_functions += 1;
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StructureMetrics {
    pub total_nodes: usize,
    pub max_depth: usize,
    pub total_functions: usize,
    pub x_prefixed_functions: usize,
    pub avg_function_bytes: f64,
    pub max_function_bytes: usize,
    pub functions_over_200_lines: usize,
}

impl StructureMetrics {
    pub fn to_code_alpha(&self) -> f64 {
        let mut score = 1.0;

        if self.functions_over_200_lines > 0 {
            score -= 0.1 * self.functions_over_200_lines as f64;
        }

        if self.max_depth > 12 {
            score -= 0.05 * (self.max_depth - 12) as f64;
        }

        if self.total_functions > 0 {
            let x_ratio = self.x_prefixed_functions as f64 / self.total_functions as f64;
            if x_ratio > 0.0 && x_ratio < 0.1 {
                score -= 0.1;
            }
        }

        score.clamp(0.0, 1.0)
    }
}
