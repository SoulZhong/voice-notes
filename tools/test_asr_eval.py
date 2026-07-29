import unittest

from asr_eval import edit_distance, evaluate, normalize, tokenize


class AsrEvalTest(unittest.TestCase):
    def test_normalize_and_edit_distance(self):
        self.assertEqual(normalize("你 好，GPT-4！"), "你好gpt4")
        self.assertEqual(edit_distance("你好", "你号"), 1)

    def test_tokenize_cjk_per_char_english_per_word(self):
        self.assertEqual(tokenize("打开 Dashboard 看 Q3 数据！"),
                         ["打", "开", "dashboard", "看", "q3", "数", "据"])
        self.assertEqual(tokenize("GPT-4 上线"), ["gpt", "4", "上", "线"])

    def test_mer_scores_english_by_word_not_char(self):
        # 纯英文:词级错误率(WER 语义)。字符级 CER 会把一个错词摊薄成极低分。
        metrics = evaluate([{"reference": "hello world", "hypothesis": "hello word"}])
        self.assertEqual(metrics["mer"], 0.5)

    def test_mer_mixed_reference_counts_tokens(self):
        # 中英混合:dashboard→dash(替换)+board(插入)= 2 处错 / 3 个参考 token。
        metrics = evaluate([{"reference": "打开 dashboard", "hypothesis": "打开 dash board"}])
        self.assertAlmostEqual(metrics["mer"], 2 / 3)
        self.assertEqual(metrics["counts"]["reference_tokens"], 3)

    def test_metrics_include_accuracy_entities_and_filter_safety(self):
        metrics = evaluate([
            {"reference": "项目叫星河", "hypothesis": "项目叫星河", "entities": ["星河"]},
            {"reference": "保留这句", "hypothesis": "保留那句", "suppressed": True},
            {"reference": "系统回声", "hypothesis": "系统回声", "suppressed": True,
             "should_suppress": True},
        ])
        self.assertEqual(metrics["counts"]["edits"], 1)
        self.assertEqual(metrics["entity_recall"], 1.0)
        self.assertEqual(metrics["filter_false_delete_rate"], 0.5)


if __name__ == "__main__":
    unittest.main()
