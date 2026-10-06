//! 题库事实与校验；默认题仅在尚未保存过题库时使用。

use gateway_plugin_sdk::{ErrorCode, PluginFault, client::HostClient};
use serde::{Deserialize, Serialize};

use crate::store;

pub const NAMESPACE: &str = "questions";
pub const MAX_BYTES: usize = 256 * 1024;
const KEY: &str = "library";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Grading {
    Exact,
    Manual,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub version: u64,
    pub title: String,
    pub prompt: String,
    pub enabled: bool,
    pub grading: Grading,
    pub answers: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Library {
    pub questions: Vec<Question>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub id: Option<String>,
    pub title: String,
    pub prompt: String,
    pub enabled: bool,
    pub grading: Grading,
    pub answers: Vec<String>,
}

impl Default for Library {
    fn default() -> Self {
        Self {
            questions: vec![
                Question {
                    id: "candy".into(), version: 1, title: "糖果题".into(), enabled: true,
                    grading: Grading::Exact, answers: vec!["21".into()],
                    prompt: "不使用任何外部工具回答以下问题：在一个黑色的袋子里放有三种口味的糖果，每种糖果有两种不同的形状（圆形和五角星形，不同的形状靠手感可以分辨）。数量如下：苹果味圆形7、桃子味圆形9、西瓜味圆形8；苹果味五角星7、桃子味五角星6、西瓜味五角星4。参赛者需在活动前决定摸出的糖果数目。问：最少取出多少个糖果，才能保证手中同时拥有不同形状的苹果味和桃子味的糖？\n只输出最终答案的阿拉伯数字，不要输出单位或解释".into(),
                },
                Question {
                    id: "bicycle-svg".into(), version: 1, title: "自行车 SVG".into(), enabled: true,
                    grading: Grading::Manual, answers: vec![],
                    prompt: "绘制一辆自行车，输出一份完整、自包含的 SVG，根节点带有 xmlns=\"http://www.w3.org/2000/svg\" 和 viewBox。不要使用外部图片、字体、脚本或工具，只输出 SVG 源码".into(),
                },
            ],
        }
    }
}

pub fn invalid(message: impl Into<String>) -> PluginFault {
    PluginFault::new(ErrorCode::InvalidInput, message)
}

pub fn check_version(actual: Option<u64>, expected: Option<u64>) -> Result<(), PluginFault> {
    if actual != expected {
        return Err(PluginFault::new(
            ErrorCode::Conflict,
            "记录已更新，请刷新后重试",
        ));
    }
    Ok(())
}

impl Library {
    pub fn save(&mut self, draft: Draft) -> Result<(), PluginFault> {
        if draft.title.trim().is_empty() || draft.prompt.trim().is_empty() {
            return Err(invalid("标题和题目不能为空"));
        }
        let answers: Vec<_> = draft
            .answers
            .into_iter()
            .map(|s| s.trim().to_owned())
            .collect();
        if draft.grading == Grading::Exact
            && (answers.is_empty() || answers.iter().any(String::is_empty))
        {
            return Err(invalid("规则题至少需要一个非空标准答案"));
        }
        if draft.grading == Grading::Manual && !answers.is_empty() {
            return Err(invalid("人工题不需要标准答案"));
        }
        let (id, version) = if let Some(id) = draft.id {
            let previous = self
                .questions
                .iter()
                .find(|q| q.id == id)
                .ok_or_else(|| invalid("题目不存在"))?;
            (
                id,
                previous
                    .version
                    .checked_add(1)
                    .ok_or_else(|| invalid("题目版本超限"))?,
            )
        } else {
            (format!("q-{:032x}", fastrand::u128(..)), 1)
        };
        let question = Question {
            id: id.clone(),
            version,
            title: draft.title.trim().into(),
            prompt: draft.prompt,
            enabled: draft.enabled,
            grading: draft.grading,
            answers,
        };
        if let Some(previous) = self.questions.iter_mut().find(|q| q.id == id) {
            *previous = question;
        } else {
            self.questions.push(question);
        }
        Ok(())
    }

    pub fn delete(&mut self, id: &str) -> Result<(), PluginFault> {
        let index = self
            .questions
            .iter()
            .position(|q| q.id == id)
            .ok_or_else(|| invalid("题目不存在"))?;
        self.questions.remove(index);
        Ok(())
    }

    pub fn choose(&self, id: Option<&str>) -> Result<Question, PluginFault> {
        if let Some(id) = id {
            return self
                .questions
                .iter()
                .find(|q| q.id == id && q.enabled)
                .cloned()
                .ok_or_else(|| invalid("题目不存在或已停用"));
        }
        let enabled: Vec<_> = self.questions.iter().filter(|q| q.enabled).collect();
        if enabled.is_empty() {
            return Err(invalid("没有启用的题目"));
        }
        Ok(enabled[fastrand::usize(..enabled.len())].clone())
    }
}

pub async fn load(host: &HostClient) -> Result<(Library, Option<u64>), PluginFault> {
    Ok(store::load_private(host, NAMESPACE, KEY)
        .await?
        .map_or_else(
            || (Library::default(), None),
            |(library, version)| (library, Some(version)),
        ))
}

pub async fn persist(
    host: &HostClient,
    library: &Library,
    version: Option<u64>,
) -> Result<u64, PluginFault> {
    store::save_private(host, NAMESPACE, KEY, library, version, MAX_BYTES).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleted_default_library_stays_empty_when_reloaded() {
        let mut library = Library::default();
        library.delete("candy").unwrap();
        library.delete("bicycle-svg").unwrap();
        let reloaded: Library =
            serde_json::from_value(serde_json::to_value(library).unwrap()).unwrap();
        assert!(reloaded.choose(None).is_err());
    }

    #[test]
    fn edits_increment_version_without_changing_test_snapshot() {
        let mut library = Library::default();
        let snapshot = library.choose(Some("candy")).unwrap();
        library
            .save(Draft {
                id: Some(snapshot.id.clone()),
                title: "修改后的题".into(),
                prompt: "另一题".into(),
                enabled: false,
                grading: Grading::Exact,
                answers: vec![" 22 ".into()],
            })
            .unwrap();
        assert_eq!(library.questions[0].version, 2);
        assert_eq!(snapshot.answers, vec!["21"]);
        assert!(library.choose(Some("candy")).is_err());
        for _ in 0..20 {
            assert_eq!(library.choose(None).unwrap().id, "bicycle-svg");
        }
    }
}
