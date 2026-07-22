import unittest

from asr_eval import edit_distance, evaluate, normalize


class AsrEvalTest(unittest.TestCase):
    def test_normalize_and_edit_distance(self):
        self.assertEqual(normalize("你 好，GPT-4！"), "你好gpt4")
        self.assertEqual(edit_distance("你好", "你号"), 1)

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
